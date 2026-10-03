use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use thiserror::Error;

use crate::artifacts::registry::{ArtifactRegistry, LocalizedArtifactRegistry};
use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::core::proc::{run_shell_async, ProcError, ProcResult};
use crate::definition::Exec;
use crate::executor::state::{BlobMode, StateData};
use crate::executor::vars;

#[derive(Error, Debug)]
pub enum ExecError {
    #[error("exec {name}: {source}")]
    Resolve {
        name: String,
        #[source]
        source: ResolveError,
    },
    #[error("exec {name}: {detail}")]
    Generic { name: String, detail: String },
    #[error("exec {name}: exited {rc}")]
    NonZeroExit {
        name: String,
        rc: i32,
        output: Option<String>,
    },
    #[error("exec {name}: artifact {uri} was not produced")]
    MissingArtifact { name: String, uri: String },
    #[error(transparent)]
    Proc(#[from] ProcError),
}

impl From<ResolveError> for ExecError {
    fn from(source: ResolveError) -> Self {
        ExecError::Resolve {
            name: String::new(),
            source,
        }
    }
}

// --- Phased execution model ---

#[derive(Debug)]
pub struct ShellResult {
    pub output: String,
    pub rc: i32,
}

#[derive(Clone)]
pub struct ExecPrepared {
    pub name: String,
    pub interpolation_map: HashMap<String, String>,
    /// bind key (trimmed, no `?`) → registered filesystem path, for command substitution.
    pub(crate) output_paths: HashMap<String, String>,
    /// (bind key, substituted URI, optional) for post-run verification.
    pub(crate) output_uris: Vec<(String, String, bool)>,
    pub cmds: Vec<String>,
    pub cwd: PathBuf,
    pub artifact_dir: PathBuf,
    pub state_dir: PathBuf,
    pub timeout: Option<f64>,
    /// The environment the commands run under.
    ///
    /// The native executor supplies a fully-resolved env (the gremlin's
    /// system variables plus anything its bootstrap script sourced). An
    /// empty env means the commands inherit the process environment.
    pub env: HashMap<String, String>,
    /// Base process environment for fallback when `env` is empty.
    /// Populated from the gremlin's runtime_config.
    pub base_env: HashMap<String, String>,
    /// Substitution env vars (`GREMLINS_<KEY> → value`) populated by
    /// `prepare_exec` for the exec command templates. Merged into the
    /// child shell's environment in `run_shell`.
    pub substitution_env: HashMap<String, String>,
    /// Per-gremlin log channel for exec stage lifecycle events.
    pub log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

/// Phase 1: resolve interpolation, compute bind paths, substitute commands.
/// Content interpolation entries are resolved against `main_registry`;
/// filepath entries and bind paths are resolved against `local_registry`.
/// Returns a fully-prepared struct that can be passed to `run_shell` and
/// `commit_exec` without further registry mutation.
pub async fn prepare_exec(
    exec: &Exec,
    main_registry: &dyn ArtifactRegistry,
    local_registry: &dyn LocalizedArtifactRegistry,
    loop_iter: &str,
    framework_subs: &HashMap<String, String>,
) -> Result<ExecPrepared, ExecError> {
    let name = &exec.name;
    let str_opts = vars::string_options(&exec.options);

    // Split interpolation: content() entries resolved against main_registry,
    // bare-URI (filepath) entries resolved against local_registry.
    let (content_map, filepath_map) =
        crate::artifacts::resolve::split_interpolation_map(&exec.interpolation_map);

    let content_interpolated = resolve_interpolation_map(main_registry, &content_map, loop_iter)
        .await
        .map_err(|e| ExecError::Resolve {
            name: name.clone(),
            source: e,
        })?;

    let filepath_interpolated = resolve_interpolation_map(local_registry, &filepath_map, loop_iter)
        .await
        .map_err(|e| ExecError::Resolve {
            name: name.clone(),
            source: e,
        })?;

    // Merge: filepath shadows content on key collision.
    let mut interpolation_map: HashMap<String, String> = content_interpolated;
    interpolation_map.extend(filepath_interpolated);

    let mut output_paths: HashMap<String, String> = HashMap::new();
    let mut output_uris: Vec<(String, String, bool)> = Vec::new();
    for (raw_key, raw_uri_str) in &exec.outputs_map {
        let k = vars::substitute_vars(raw_key, &str_opts, &interpolation_map, framework_subs);
        let optional = k.ends_with('?');
        let key = k.trim_end_matches('?').to_string();
        let mut uri_str =
            vars::substitute_vars(raw_uri_str, &str_opts, &interpolation_map, framework_subs);
        if !loop_iter.is_empty() {
            uri_str = uri_str.replace("{loop_iter}", loop_iter);
        }
        let uri = Uri::parse(&uri_str).map_err(|e| ExecError::Generic {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        // Optional binds are skipped when a sibling already committed the URI.
        // Check against the main registry (the authority for what exists).
        if !optional && main_registry.is_registered(&uri_str).await {
            return Err(ExecError::Generic {
                name: name.clone(),
                detail: format!("artifact {uri_str:?} is already produced — duplicate producer"),
            });
        }
        let path = local_registry
            .path_for_uri(&uri)
            .await
            .map_err(|e| ExecError::Generic {
                name: name.clone(),
                detail: e.to_string(),
            })?;
        output_paths.insert(key.clone(), path);
        output_uris.push((key, uri_str, optional));
    }

    // Merge interpolation_map and output_paths (bind shadows interpolation)
    let subst_vars: HashMap<String, String> = interpolation_map
        .iter()
        .chain(output_paths.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let raw_cmds: Vec<String> = exec
        .options
        .get("cmds")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // Substitute {key} tokens with $GREMLINS_<KEY> env-var references.
    // All commands share one env map so the same key always maps to the
    // same env var name.
    let mut substitution_env: HashMap<String, String> = HashMap::new();
    let mut key_to_env: HashMap<String, String> = HashMap::new();
    let mut used_names: HashMap<String, u32> = HashMap::new();
    let cmds: Vec<String> = raw_cmds
        .iter()
        .map(|c| {
            vars::substitute_vars_to_env(
                c,
                &str_opts,
                &subst_vars,
                framework_subs,
                &mut substitution_env,
                &mut key_to_env,
                &mut used_names,
            )
        })
        .collect();

    let timeout: Option<f64> = exec.options.get("timeout").and_then(|v| v.as_f64());

    Ok(ExecPrepared {
        name: name.clone(),
        interpolation_map,
        output_paths,
        output_uris,
        cmds,
        cwd: PathBuf::new(),
        artifact_dir: PathBuf::new(),
        state_dir: PathBuf::new(),
        timeout,
        env: HashMap::new(),
        base_env: HashMap::new(),
        substitution_env,
        log_tx: None,
    })
}

/// Sanitize a stage name for use as a log filename component.
///
/// Replaces path separators, `..`, and other dangerous characters with `_`.
/// The result is safe to embed in a file path without escaping the parent
/// directory.
fn sanitize_log_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
            _ => '_',
        })
        .collect::<String>()
        // Collapse ".." sequences that survived individual-char replacement.
        .replace("..", "__")
}

/// Resolve `${VAR}` references in `s` using the substitution env.
/// `${GREMLINS_FOO}` → the value of `GREMLINS_FOO` in the env map;
/// unknown variables are left as-is.
fn resolve_cmd_for_log(s: &str, env: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(dollar) = rest.find("${") {
        result.push_str(&rest[..dollar]);
        rest = &rest[dollar + 2..];
        let close = rest.find('}');
        match close {
            Some(end) => {
                let var = &rest[..end];
                if let Some(val) = env.get(var) {
                    result.push_str(val);
                } else {
                    // Unknown variable — keep the `${VAR}` verbatim.
                    result.push_str("${");
                    result.push_str(&rest[..end + 1]);
                }
                rest = &rest[end + 1..];
            }
            None => {
                // Unclosed `${` — put back what we consumed.
                result.push_str("${");
                break;
            }
        }
    }
    result.push_str(rest);
    result
}

/// Build a single bash script that executes every command in `cmds` within one
/// shell session, preserving shell state (variables, `cd`, functions, `umask`,
/// etc.) across command boundaries. Each command is wrapped with `printf`
/// header/footer markers so the stream log shows which command produced which
/// output and which command failed or timed out. The script stops on the first
/// non-zero exit (mirroring `&&` semantics).
fn build_instrumented_script(
    cmds: &[String],
    substitution_env: &HashMap<String, String>,
) -> String {
    let total = cmds.len();
    let mut script = String::with_capacity(cmds.iter().map(|c| c.len() + 80).sum());
    script.push_str("set +e\n");
    for (i, cmd) in cmds.iter().enumerate() {
        let resolved = resolve_cmd_for_log(cmd, substitution_env);
        // Escape single quotes for embedding in a single-quoted shell string:
        // ' → '\''.
        let escaped = resolved.replace('\'', "'\\''");
        // The raw command is embedded verbatim so bash resolves ${…}
        // references and other shell syntax at runtime.
        script.push_str(&format!(
            "printf -- '\\n--- cmd {i1}/{total}: %s ---\\n' '{escaped}'\n",
            i1 = i + 1,
        ));
        script.push_str(cmd);
        script.push('\n');
        script.push_str(&format!(
            "_rc=$?\nprintf -- '--- cmd {i1}/{total} exit: %d ---\\n' \"$_rc\"\n",
            i1 = i + 1,
        ));
        script.push_str("if [ \"$_rc\" -ne 0 ]; then exit \"$_rc\"; fi\n");
    }
    script
}

/// Phase 2: run the shell commands. Uses only the prepared data; no registry access.
pub async fn run_shell(
    prepared: &ExecPrepared,
    state: &StateData,
) -> Result<ShellResult, ExecError> {
    if prepared.cmds.is_empty() {
        return Ok(ShellResult {
            output: String::new(),
            rc: 0,
        });
    }

    // A prepared env is authoritative when present; otherwise fall back to
    // the base process env snapshotted from the gremlin's runtime_config.
    let mut env: HashMap<String, String> = if prepared.env.is_empty() {
        prepared.base_env.clone()
    } else {
        prepared.env.clone()
    };
    // Merge substitution env vars (GREMLINS_<KEY> → value) into the child
    // shell's environment so {key} tokens resolve verbatim.
    for (k, v) in &prepared.substitution_env {
        env.insert(k.clone(), v.clone());
    }

    let safe_name = sanitize_log_filename(&prepared.name);
    let blob_name = format!("exec_stage_logs/exec-{safe_name}.log");

    // Open the stream file through the state store, which creates parent
    // directories and rejects traversal / absolute paths.
    // We also compute the filesystem path for run_shell_async's streaming.
    let stream_path = prepared.state_dir.join(&blob_name);

    let mut stream_blob: Option<Box<dyn std::io::Write + Send>> =
        match state.open(&blob_name, BlobMode::Append) {
            Ok(blob) => {
                if let Some(ref tx) = prepared.log_tx {
                    let _ = tx.send(format!(
                        "exec {}: streaming output to {}",
                        prepared.name, blob_name
                    ));
                }
                Some(blob)
            }
            Err(e) => {
                if let Some(ref tx) = prepared.log_tx {
                    let _ = tx.send(format!(
                        "exec {}: failed to open exec stage log: {e}",
                        prepared.name
                    ));
                }
                None
            }
        };

    // Write stage header to stream file (best-effort).
    if let Some(ref mut blob) = stream_blob {
        let header = format!(
            "=== exec stage: {} ===\ncwd: {}\ncmds: {}\n",
            prepared.name,
            prepared.cwd.display(),
            prepared.cmds.len(),
        );
        let _ = blob.write_all(header.as_bytes());
    }

    // Run commands as a single instrumented shell script so shell state
    // (variables, cd, functions, umask, etc.) is preserved across command
    // boundaries.  Per-command headers and footers are emitted by the
    // script itself so they appear in the stream log interleaved with the
    // output they frame.
    let stage_start = std::time::Instant::now();

    // Validate timeout before computing a deadline —
    // Duration::from_secs_f64 panics on NaN, negative, or overflow values.
    if let Some(t) = prepared.timeout {
        if !t.is_finite() || t < 0.0 || t > std::time::Duration::MAX.as_secs_f64() {
            return Err(ExecError::Proc(ProcError::InvalidTimeout(t)));
        }
    }

    let script = build_instrumented_script(&prepared.cmds, &prepared.substitution_env);

    if let Some(ref tx) = prepared.log_tx {
        let _ = tx.send(format!(
            "exec {}: running {} command(s) via instrumented script (timeout={:?})",
            prepared.name,
            prepared.cmds.len(),
            prepared.timeout,
        ));
    }

    // Pass the filesystem path to run_shell_async for streaming stdout/stderr.
    // The state store already created parent directories via state.open() above.
    let stream_path_arg: Option<&std::path::Path> = if stream_blob.is_some() {
        Some(&stream_path)
    } else {
        None
    };

    let result = run_shell_async(
        &script,
        Some(&prepared.cwd),
        Some(&env),
        prepared.timeout,
        stream_path_arg,
    )
    .await;

    let elapsed = stage_start.elapsed();

    match result {
        Ok(r) => {
            let combined_output = format!(
                "{}{}",
                String::from_utf8_lossy(&r.stdout),
                String::from_utf8_lossy(&r.stderr),
            );

            // Stage footer.
            if let Some(ref mut blob) = stream_blob {
                let footer = format!(
                    "--- exit: {} (duration: {:.1}s) ---\n",
                    r.returncode,
                    elapsed.as_secs_f64()
                );
                let _ = blob.write_all(footer.as_bytes());
            }

            if r.returncode != 0 {
                return Err(ExecError::NonZeroExit {
                    name: prepared.name.clone(),
                    rc: r.returncode,
                    output: Some(combined_output.trim().to_string()),
                });
            }

            Ok(ShellResult {
                output: combined_output.trim().to_string(),
                rc: r.returncode,
            })
        }
        Err(e) => {
            // Stage footer on error.
            if let Some(ref mut blob) = stream_blob {
                let footer = format!(
                    "--- exit: error (duration: {:.1}s) ---\n",
                    elapsed.as_secs_f64()
                );
                let _ = blob.write_all(footer.as_bytes());
            }

            Err(ExecError::Proc(e))
        }
    }
}

/// Post-process a ProcResult into a ShellResult (log writing, bail detection).
pub fn process_shell_result(
    prepared: &ExecPrepared,
    result: ProcResult,
) -> Result<ShellResult, ExecError> {
    let name = &prepared.name;

    let raw_output = {
        let mut buf = result.stdout.clone();
        buf.extend_from_slice(&result.stderr);
        buf
    };
    let raw_output_str = String::from_utf8_lossy(&raw_output).to_string();
    let shell_output = raw_output_str.trim().to_string();
    let shell_rc = result.returncode;

    if let Some(ref tx) = prepared.log_tx {
        let _ = tx.send(format!(
            "exec {name}: done rc={shell_rc} output_len={}",
            raw_output_str.len(),
        ));
    }

    // A non-zero exit is always an error.
    if shell_rc != 0 {
        return Err(ExecError::NonZeroExit {
            name: name.clone(),
            rc: shell_rc,
            output: Some(shell_output.clone()),
        });
    }

    Ok(ShellResult {
        output: shell_output,
        rc: shell_rc,
    })
}

/// Phase 3: commit produced artifacts into the localized registry.
/// Non-optional artifacts that are absent abort the stage, except bail URIs.
pub async fn commit_exec(
    prepared: &ExecPrepared,
    local_registry: &dyn LocalizedArtifactRegistry,
) -> Result<(), ExecError> {
    for (key, uri_str, optional) in &prepared.output_uris {
        let path = &prepared.output_paths[key];
        let produced = local_registry.has_file(path).await;
        if produced {
            local_registry
                .commit(uri_str, path)
                .await
                .map_err(|e| ExecError::Generic {
                    name: prepared.name.clone(),
                    detail: e.to_string(),
                })?;
        } else if !*optional {
            return Err(ExecError::MissingArtifact {
                name: prepared.name.clone(),
                uri: uri_str.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::registry::FileSystemArtifactRegistry;
    use crate::executor::state::StateData;
    use std::fs;
    use std::path::Path;

    #[tokio::test]
    async fn test_commit_exec_optional_bind_ignores_duplicate_registration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let registry = FileSystemArtifactRegistry::new(artifact_dir);

        // A sibling already committed this URI.
        let uri = Uri::parse("artifact://out.txt").unwrap();
        registry
            .write_into_registry(&uri, "existing")
            .await
            .unwrap();

        let fw = HashMap::new();

        // Optional bind: skipped, not an error.
        let optional_exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("out?".to_string(), "artifact://out.txt".to_string())]),
        };
        let prepared = prepare_exec(&optional_exec, &registry, &registry, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.output_uris[0].0, "out");
        assert!(prepared.output_uris[0].2);

        // Non-optional bind: still a duplicate-producer error.
        let non_optional_exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let err = prepare_exec(&non_optional_exec, &registry, &registry, "", &fw)
            .await
            .err()
            .expect("expected duplicate-producer error");
        assert!(matches!(err, ExecError::Generic { .. }));
    }

    #[tokio::test]
    async fn test_commit_exec_missing_non_optional_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let registry = FileSystemArtifactRegistry::new(artifact_dir);

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &registry, &registry, "", &fw)
            .await
            .unwrap();
        let err = commit_exec(&prepared, &registry).await.unwrap_err();
        assert!(matches!(err, ExecError::MissingArtifact { .. }));
        assert!(!registry.is_registered("artifact://out.txt").await);
    }

    #[tokio::test]
    async fn test_commit_exec_allows_missing_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let registry = FileSystemArtifactRegistry::new(artifact_dir);

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("out?".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &registry, &registry, "", &fw)
            .await
            .unwrap();
        commit_exec(&prepared, &registry).await.unwrap();
        assert!(!registry.is_registered("artifact://out.txt").await);
    }

    #[tokio::test]
    async fn test_commit_exec_registers_produced_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let registry = FileSystemArtifactRegistry::new(artifact_dir);

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &registry, &registry, "", &fw)
            .await
            .unwrap();
        fs::write(&prepared.output_paths["out"], "data").unwrap();
        commit_exec(&prepared, &registry).await.unwrap();
        assert!(registry.is_registered("artifact://out.txt").await);
    }

    #[tokio::test]
    async fn test_commit_exec_dry_run_succeeds_without_files() {
        let reg = crate::artifacts::registry::DryRunArtifactRegistry::new();

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &reg, &reg, "", &fw).await.unwrap();
        // No file written — DryRunArtifactRegistry::has_file always returns true.
        commit_exec(&prepared, &reg).await.unwrap();
        assert!(reg.is_registered("artifact://out.txt").await);
    }

    // --- run_shell integration: injection payloads are not executed ---

    #[tokio::test]
    async fn test_run_shell_injection_payload_not_executed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();

        // A payload with backticks and $() that would execute if the value
        // were interpolated literally into the shell command.
        let injection = "`echo pwned > /tmp/gremlins_injection_test_marker`; $(touch /tmp/gremlins_injection_test_marker2)";
        let mut substitution_env = HashMap::new();
        substitution_env.insert("GREMLINS_PR_TITLE".to_string(), injection.to_string());

        let prepared = ExecPrepared {
            name: "injection-test".to_string(),
            interpolation_map: HashMap::new(),
            output_paths: HashMap::new(),
            output_uris: Vec::new(),
            cmds: vec!["printf '%s' \"${GREMLINS_PR_TITLE}\"".to_string()],
            cwd: tmp.path().to_path_buf(),
            artifact_dir: artifact_dir.clone(),
            state_dir: state_dir.clone(),
            timeout: Some(5.0),
            env: HashMap::new(),
            base_env: HashMap::new(),
            substitution_env,
            log_tx: None,
        };

        let state = StateData::with_state_dir(&state_dir);

        let result = run_shell(&prepared, &state).await.unwrap();
        // The output must contain the literal payload — not execute it.
        // (Per-command markers are now part of the combined output.)
        assert!(result.output.contains(injection));
        // Neither marker file must exist.
        assert!(!Path::new("/tmp/gremlins_injection_test_marker").exists());
        assert!(!Path::new("/tmp/gremlins_injection_test_marker2").exists());
    }

    // --- run_shell header/footer framing ---

    fn make_prepared(name: &str, cmds: Vec<&str>, state_dir: &Path) -> ExecPrepared {
        ExecPrepared {
            name: name.to_string(),
            interpolation_map: HashMap::new(),
            output_paths: HashMap::new(),
            output_uris: Vec::new(),
            cmds: cmds.into_iter().map(|s| s.to_string()).collect(),
            cwd: std::env::current_dir().unwrap(),
            artifact_dir: state_dir.join("artifacts"),
            state_dir: state_dir.to_path_buf(),
            timeout: Some(5.0),
            env: HashMap::new(),
            base_env: HashMap::new(),
            substitution_env: HashMap::new(),
            log_tx: None,
        }
    }

    fn read_log(state_dir: &Path, name: &str) -> String {
        let safe = sanitize_log_filename(name);
        let path = state_dir
            .join("exec_stage_logs")
            .join(format!("exec-{safe}.log"));
        fs::read_to_string(&path).unwrap_or_default()
    }

    #[tokio::test]
    async fn test_run_shell_resolves_env_vars_in_command_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let mut substitution_env = HashMap::new();
        substitution_env.insert("GREMLINS_INPUT".to_string(), "/tmp/input.md".to_string());
        let prepared = ExecPrepared {
            name: "resolved".to_string(),
            cmds: vec!["echo \"${GREMLINS_INPUT}\"".to_string()],
            substitution_env,
            log_tx: None,
            ..make_prepared("resolved", vec!["echo ok"], &state_dir)
        };
        let state = StateData::with_state_dir(&state_dir);
        let result = run_shell(&prepared, &state).await.unwrap();
        assert!(result.output.contains("/tmp/input.md"));

        let log = read_log(&state_dir, "resolved");
        assert!(
            log.contains("cmd 1/1: echo \"/tmp/input.md\""),
            "log should contain resolved command, got: {log}"
        );
    }

    #[tokio::test]
    async fn test_run_shell_writes_header_and_footer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("hello", vec!["echo UNIQUE_OUTPUT_MARKER"], &state_dir);
        let state = StateData::with_state_dir(&state_dir);
        let result = run_shell(&prepared, &state).await.unwrap();
        assert!(result.output.contains("UNIQUE_OUTPUT_MARKER"));
        assert_eq!(result.rc, 0);

        let log = read_log(&state_dir, "hello");
        assert!(
            log.contains("=== exec stage: hello ==="),
            "missing header: {log}"
        );
        assert!(log.contains("cwd:"), "missing cwd: {log}");
        assert!(log.contains("cmds: 1"), "missing cmds count: {log}");
        assert!(
            log.contains("--- cmd 1/1: echo UNIQUE_OUTPUT_MARKER ---"),
            "missing cmd header: {log}"
        );
        assert!(
            log.contains("UNIQUE_OUTPUT_MARKER"),
            "missing command output: {log}"
        );
        assert!(
            log.contains("--- cmd 1/1 exit: 0"),
            "missing cmd footer: {log}"
        );
        assert!(log.contains("--- exit: 0"), "missing stage footer: {log}");
        assert!(log.contains("duration:"), "missing duration: {log}");

        // Verify ordering: header → cmd header → output → cmd footer → stage footer.
        let header_pos = log.find("=== exec stage: hello ===").unwrap();
        let cmd_header_pos = log
            .find("--- cmd 1/1: echo UNIQUE_OUTPUT_MARKER ---")
            .unwrap();
        let output_pos = log.find("UNIQUE_OUTPUT_MARKER\n").unwrap();
        let cmd_footer_pos = log.find("--- cmd 1/1 exit: 0").unwrap();
        let footer_pos = log.find("--- exit: 0").unwrap();
        assert!(header_pos < cmd_header_pos);
        assert!(cmd_header_pos < output_pos);
        assert!(output_pos < cmd_footer_pos);
        assert!(cmd_footer_pos < footer_pos);
    }

    #[tokio::test]
    async fn test_run_shell_header_footer_with_no_child_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("silent", vec!["true"], &state_dir);
        let state = StateData::with_state_dir(&state_dir);
        let result = run_shell(&prepared, &state).await.unwrap();
        // Output now includes per-command markers from the instrumented script.
        assert!(result.output.contains("--- cmd 1/1: true ---"));
        assert!(result.output.contains("--- cmd 1/1 exit: 0"));
        assert_eq!(result.rc, 0);

        let log = read_log(&state_dir, "silent");
        assert!(
            log.contains("=== exec stage: silent ==="),
            "missing header: {log}"
        );
        assert!(
            log.contains("--- cmd 1/1: true ---"),
            "missing cmd header: {log}"
        );
        assert!(
            log.contains("--- cmd 1/1 exit: 0"),
            "missing cmd footer: {log}"
        );
        assert!(log.contains("--- exit: 0"), "missing stage footer: {log}");
    }

    #[tokio::test]
    async fn test_run_shell_footer_shows_nonzero_exit_code() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("failing", vec!["exit 42"], &state_dir);
        let state = StateData::with_state_dir(&state_dir);

        let err = run_shell(&prepared, &state).await.unwrap_err();
        assert!(matches!(err, ExecError::NonZeroExit { rc: 42, .. }));

        let log = read_log(&state_dir, "failing");
        assert!(
            log.contains("--- exit: 42"),
            "footer missing exit 42: {log}"
        );
    }

    #[tokio::test]
    async fn test_run_shell_repeated_invocations_append() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared1 = make_prepared("repeat", vec!["echo RUN_ONE"], &state_dir);
        let prepared2 = make_prepared("repeat", vec!["echo RUN_TWO"], &state_dir);
        let state = StateData::with_state_dir(&state_dir);
        run_shell(&prepared1, &state).await.unwrap();
        run_shell(&prepared2, &state).await.unwrap();

        let log = read_log(&state_dir, "repeat");
        // Both runs appear.
        let first_header = log.find("=== exec stage: repeat ===");
        let second_header = log.rfind("=== exec stage: repeat ===");
        assert!(first_header.is_some());
        assert!(second_header.is_some());
        assert!(first_header.unwrap() < second_header.unwrap());

        let first_exit = log.find("--- exit: 0");
        let second_exit = log.rfind("--- exit: 0");
        assert!(first_exit.is_some());
        assert!(second_exit.is_some());
        assert!(first_exit.unwrap() < second_exit.unwrap());

        assert!(log.contains("RUN_ONE"));
        assert!(log.contains("RUN_TWO"));
    }

    #[tokio::test]
    async fn test_run_shell_succeeds_when_log_dir_cannot_be_created() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create a file where the log directory would be, so create_dir_all fails.
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let log_dir = state_dir.join("exec_stage_logs");
        fs::write(&log_dir, "block").unwrap(); // file, not dir — create_dir_all will fail

        let prepared = make_prepared("resilient", vec!["echo still-works"], &state_dir);
        let state = StateData::with_state_dir(&state_dir);
        let result = run_shell(&prepared, &state).await.unwrap();
        assert!(result.output.contains("still-works"));
        assert_eq!(result.rc, 0);
        // The log file was never created.
        let safe = sanitize_log_filename("resilient");
        let log_path = log_dir.join(format!("exec-{safe}.log"));
        assert!(!log_path.exists());
    }

    #[tokio::test]
    async fn test_run_shell_sanitizes_dangerous_stage_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("../../../etc/passwd", vec!["echo ok"], &state_dir);
        let state = StateData::with_state_dir(&state_dir);
        let result = run_shell(&prepared, &state).await.unwrap();
        assert!(result.output.contains("ok"));

        // The log file must be inside exec_stage_logs, not escaped.
        let log_dir = state_dir.join("exec_stage_logs");
        let safe = sanitize_log_filename("../../../etc/passwd");
        let log_path = log_dir.join(format!("exec-{safe}.log"));
        assert!(log_path.exists());
        // The path must not escape the log dir.
        let canon_log_dir = log_dir.canonicalize().unwrap();
        let canon_log = log_path.canonicalize().unwrap();
        assert!(canon_log.starts_with(&canon_log_dir));
    }

    // --- resolve_cmd_for_log ---

    #[test]
    fn test_resolve_cmd_for_log_substitutes_known_vars() {
        let env = HashMap::from([
            ("GREMLINS_PLAN".to_string(), "/tmp/plan.md".to_string()),
            ("GREMLINS_ISSUE".to_string(), "42".to_string()),
        ]);
        let resolved = resolve_cmd_for_log(
            "gh publish \"${GREMLINS_PLAN}\" > \"${GREMLINS_ISSUE}\"",
            &env,
        );
        assert_eq!(resolved, "gh publish \"/tmp/plan.md\" > \"42\"");
    }

    #[test]
    fn test_resolve_cmd_for_log_unknown_var_left_as_is() {
        let env = HashMap::new();
        let resolved = resolve_cmd_for_log("echo ${NOT_SET}", &env);
        assert_eq!(resolved, "echo ${NOT_SET}");
    }

    #[test]
    fn test_resolve_cmd_for_log_no_braces_untouched() {
        let env = HashMap::from([("GREMLINS_X".to_string(), "val".to_string())]);
        // $VAR without braces is not the pattern the substitution layer emits.
        let resolved = resolve_cmd_for_log("echo $GREMLINS_X", &env);
        assert_eq!(resolved, "echo $GREMLINS_X");
    }

    #[test]
    fn test_resolve_cmd_for_log_unclosed_brace_kept_verbatim() {
        let env = HashMap::new();
        let resolved = resolve_cmd_for_log("echo ${oops", &env);
        assert_eq!(resolved, "echo ${oops");
    }

    #[test]
    fn test_sanitize_log_filename_replaces_dangerous_chars() {
        assert_eq!(sanitize_log_filename("hello"), "hello");
        assert_eq!(sanitize_log_filename("a/b"), "a_b");
        assert_eq!(sanitize_log_filename("a\\b"), "a_b");
        assert_eq!(sanitize_log_filename(".."), "__");
        assert_eq!(sanitize_log_filename("a..b"), "a__b");
        assert_eq!(sanitize_log_filename("a\0b"), "a_b");
        assert_eq!(sanitize_log_filename("hello world"), "hello_world");
        assert_eq!(sanitize_log_filename("a.b-c_d"), "a.b-c_d");
    }
}
