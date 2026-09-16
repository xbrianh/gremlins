//! Sourcing bash environment scripts in an isolated subprocess.
//!
//! A gremlin's bootstrap script is arbitrary bash, and the environment it
//! leaves behind is what its stage processes inherit. Sourcing it in-process
//! would let the script mutate ours, so instead we hand it to a `bash` child
//! whose entire environment *we* choose ([`load_env_file_isolated`]) and read
//! back what the script made of it. The caller re-injects its own system
//! variables afterwards, so a script can shape the environment but cannot
//! tamper with the gremlin's own.
//!
//! [`source_env_string`] is the same operation for a script that arrives as a
//! string rather than a path: it spills the text to a temporary file — bash's
//! `source` needs one — and reuses the path-based loader.
//!
//! The process plumbing lives in [`crate::core::proc`]; this module contributes
//! the argv, the environment handling, and the parse of `env -0`.

use std::collections::HashMap;
use std::io;
use std::io::Write;
use std::path::Path;

use crate::core::proc;

/// Variables bash sets for itself, which say nothing about what the script
/// wanted and so must not leak into the sourced environment.
static BASH_INTERNALS: &[&str] = &[
    "_",
    "BASH",
    "BASH_VERSION",
    "BASH_VERSINFO",
    "BASHOPTS",
    "BASHPID",
    "PPID",
    "SHLVL",
    "SHELLOPTS",
];

/// Failure modes of sourcing an environment script.
///
/// The pyext layer raises every one of these as a Python `RuntimeError`, the
/// same shape the Python module this replaces always produced.
#[derive(Debug, thiserror::Error)]
pub enum EnvFileError {
    /// `bash` could not be found on `PATH`.
    #[error("failed to source {path}: bash not found")]
    BashNotFound { path: String },
    /// `bash` ran, but the script exited non-zero; `stderr` is its (trimmed)
    /// complaint.
    #[error("failed to source {path}:\n{stderr}")]
    SourceFailed { path: String, stderr: String },
    /// Anything else: a spawn or filesystem failure, already described.
    #[error("{0}")]
    Io(String),
}

/// Source `path` under `base_env` and return the environment it produces.
///
/// `base_env` is the *complete* environment the `bash` child receives — the
/// caller builds it (typically the parent environment plus system variables) so
/// the script has full control over what to keep or override. Nothing else is
/// inherited. `BASH_ENV` is stripped regardless, so bash does not auto-source
/// an unrelated file behind the caller's back.
pub fn load_env_file_isolated(
    path: &Path,
    base_env: &HashMap<String, String>,
    cwd: Option<&Path>,
) -> Result<HashMap<String, String>, EnvFileError> {
    let path_label = path.display().to_string();

    let env: HashMap<String, String> = base_env
        .iter()
        .filter(|(key, _)| key.as_str() != "BASH_ENV")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    // `source "$1"` keeps the path out of the script text, so a path with
    // spaces or shell metacharacters needs no quoting. `env -0` separates each
    // entry with NUL, which survives values that contain newlines.
    let argv = vec![
        "bash".to_string(),
        "-c".to_string(),
        r#"source "$1" >/dev/null && env -0"#.to_string(),
        "--".to_string(),
        path_label.clone(),
    ];

    let result = proc::run_with_env(&argv, cwd, &env).map_err(|err| match err {
        proc::ProcError::Io(e) if e.kind() == io::ErrorKind::NotFound => {
            EnvFileError::BashNotFound {
                path: path_label.clone(),
            }
        }
        other => EnvFileError::Io(format!("failed to source {path_label}: {other}")),
    })?;

    if result.returncode != 0 {
        return Err(EnvFileError::SourceFailed {
            path: path_label,
            stderr: String::from_utf8_lossy(&result.stderr).trim().to_string(),
        });
    }

    Ok(parse_env_output(&result.stdout))
}

/// Source a bash script held in memory and return the environment it produces.
///
/// The script is written to a temporary file because bash's `source` builtin
/// reads from a path; the file is removed when this function returns, whatever
/// the outcome.
pub fn source_env_string(
    script: &str,
    base_env: &HashMap<String, String>,
    cwd: Option<&Path>,
) -> Result<HashMap<String, String>, EnvFileError> {
    let mut file = tempfile::Builder::new()
        .prefix("gremlins-env")
        .suffix(".env.sh")
        .tempfile()
        .map_err(|e| EnvFileError::Io(format!("failed to write bootstrap script: {e}")))?;

    file.write_all(script.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| EnvFileError::Io(format!("failed to write bootstrap script: {e}")))?;

    // `file` is a `NamedTempFile`: its `Drop` unlinks the file, so the cleanup
    // is the scope itself — no explicit unlink, and no leak if the load fails.
    load_env_file_isolated(file.path(), base_env, cwd)
}

/// Parse the NUL-delimited output of `env -0` into a map, dropping the
/// [bash internals](BASH_INTERNALS).
///
/// Entries without an `=` are ignored, as are malformed bytes: `env` output is
/// not ours, so a stray byte decodes to U+FFFD rather than failing the load.
fn parse_env_output(stdout: &[u8]) -> HashMap<String, String> {
    let mut env = HashMap::new();
    for entry in stdout.split(|byte| *byte == 0) {
        let decoded = String::from_utf8_lossy(entry);
        if let Some((key, value)) = decoded.split_once('=') {
            env.insert(key.to_string(), value.to_string());
        }
    }
    for key in BASH_INTERNALS {
        env.remove(*key);
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parse_reads_every_entry() {
        let stdout = b"FOO=bar\0PATH=/bin\0";
        assert_eq!(
            parse_env_output(stdout),
            env_of(&[("FOO", "bar"), ("PATH", "/bin")])
        );
    }

    #[test]
    fn parse_keeps_its_split_at_the_first_equals() {
        let stdout = b"FOO=a=b=c\0";
        assert_eq!(parse_env_output(stdout), env_of(&[("FOO", "a=b=c")]));
    }

    #[test]
    fn parse_keeps_values_containing_newlines() {
        let stdout = b"FOO=one\ntwo\0";
        assert_eq!(parse_env_output(stdout), env_of(&[("FOO", "one\ntwo")]));
    }

    #[test]
    fn parse_drops_bash_internals() {
        let stdout = b"_=/usr/bin/env\0BASH=/bin/bash\0SHLVL=1\0FOO=bar\0";
        assert_eq!(parse_env_output(stdout), env_of(&[("FOO", "bar")]));
    }

    #[test]
    fn parse_ignores_entries_without_an_equals() {
        assert!(parse_env_output(b"NOISE\0\0").is_empty());
    }

    #[test]
    fn parse_replaces_malformed_bytes() {
        assert_eq!(
            parse_env_output(b"FOO=\xff\0"),
            env_of(&[("FOO", "\u{FFFD}")])
        );
    }
}
