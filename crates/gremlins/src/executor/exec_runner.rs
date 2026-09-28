use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use thiserror::Error;

use crate::artifacts::registry::{ArtifactRegistry, LocalizedArtifactRegistry};
use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::core::proc::{run_shell_async, ProcError, ProcResult};
use crate::definition::Exec;
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
    pub(crate) bind_paths: HashMap<String, String>,
    /// (bind key, substituted URI, optional) for post-run verification.
    pub(crate) bind_uris: Vec<(String, String, bool)>,
    pub cmds: Vec<String>,
    pub cwd: PathBuf,
    pub artifact_dir: PathBuf,
    pub state_dir: PathBuf,
    pub timeout: Option<f64>,
    /// The environment the commands run under.
    ///
    /// The native executor supplies a fully-resolved env (the gremlin's
    /// system variables plus anything its bootstrap script sourced); the
    /// pyext path leaves it empty and the commands inherit the process
    /// environment instead.
    pub env: HashMap<String, String>,
    /// Substitution env vars (`GREMLINS_<KEY> → value`) populated by
    /// `prepare_exec` for the exec command templates. Merged into the
    /// child shell's environment in `run_shell`.
    pub substitution_env: HashMap<String, String>,
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

    let mut bind_paths: HashMap<String, String> = HashMap::new();
    let mut bind_uris: Vec<(String, String, bool)> = Vec::new();
    for (raw_key, raw_uri_str) in &exec.bind_map {
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
        bind_paths.insert(key.clone(), path);
        bind_uris.push((key, uri_str, optional));
    }

    // Merge interpolation_map and bind_paths (bind shadows interpolation)
    let subst_vars: HashMap<String, String> = interpolation_map
        .iter()
        .chain(bind_paths.iter())
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
        bind_paths,
        bind_uris,
        cmds,
        cwd: PathBuf::new(),
        artifact_dir: PathBuf::new(),
        state_dir: PathBuf::new(),
        timeout,
        env: HashMap::new(),
        substitution_env,
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

/// Phase 2: run the shell commands. Uses only the prepared data; no registry access.
pub async fn run_shell(prepared: &ExecPrepared) -> Result<ShellResult, ExecError> {
    if prepared.cmds.is_empty() {
        return Ok(ShellResult {
            output: String::new(),
            rc: 0,
        });
    }

    let joined = prepared.cmds.join(" && ");
    // A prepared env is authoritative when present; otherwise inherit ours.
    let mut env: HashMap<String, String> = if prepared.env.is_empty() {
        std::env::vars().collect()
    } else {
        prepared.env.clone()
    };
    // Merge substitution env vars (GREMLINS_<KEY> → value) into the child
    // shell's environment so {key} tokens resolve verbatim.
    for (k, v) in &prepared.substitution_env {
        env.insert(k.clone(), v.clone());
    }

    let log_dir = prepared.state_dir.join("exec_stage_logs");
    let safe_name = sanitize_log_filename(&prepared.name);
    let stream_path = log_dir.join(format!("exec-{safe_name}.log"));

    // Defend against path traversal: after sanitization, the resolved path
    // must still be a child of the log directory.
    let stream_path_arg: Option<&std::path::Path> = match std::fs::create_dir_all(&log_dir) {
        Ok(()) => {
            // Canonicalize the log dir so we can check containment.
            let canonical_log_dir = log_dir.canonicalize().ok();

            let contained = canonical_log_dir.as_ref().is_some_and(|canon_dir| {
                // Resolve stream_path. The file may not exist yet, so
                // canonicalize its parent and join the filename.
                let resolved = stream_path.canonicalize().ok().unwrap_or_else(|| {
                    stream_path
                        .parent()
                        .and_then(|p| p.canonicalize().ok())
                        .map(|parent| parent.join(stream_path.file_name().unwrap_or_default()))
                        .unwrap_or_default()
                });
                resolved.starts_with(canon_dir)
            });

            if !contained {
                log::warn!(
                    "exec {}: stream path escapes log dir, skipping stream",
                    prepared.name
                );
                None
            } else {
                log::info!(
                    "exec {}: streaming output to {}",
                    prepared.name,
                    stream_path.display()
                );
                Some(&stream_path)
            }
        }
        Err(e) => {
            log::warn!(
                "exec {}: failed to create exec_stage_logs dir: {e}",
                prepared.name
            );
            None
        }
    };

    // Write header to stream file (best-effort).
    if stream_path_arg.is_some() {
        let header = format!(
            "=== exec stage: {} ===\ncwd: {}\ncommand: {}\n--- output ---\n",
            prepared.name,
            prepared.cwd.display(),
            joined
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stream_path)
            .and_then(|mut f| f.write_all(header.as_bytes()));
    }

    let start = std::time::Instant::now();
    let result = run_shell_async(
        &joined,
        Some(&prepared.cwd),
        Some(&env),
        prepared.timeout,
        stream_path_arg,
    )
    .await;
    let elapsed = start.elapsed();

    // Append footer to stream file (best-effort).
    if stream_path_arg.is_some() {
        let footer = match &result {
            Ok(r) => format!(
                "\n--- exit: {} (duration: {:.1}s) ---\n",
                r.returncode,
                elapsed.as_secs_f64()
            ),
            Err(_) => format!(
                "\n--- exit: error (duration: {:.1}s) ---\n",
                elapsed.as_secs_f64()
            ),
        };
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .open(&stream_path)
            .and_then(|mut f| f.write_all(footer.as_bytes()));
    }

    process_shell_result(prepared, result?)
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

    log::info!(
        "exec {name}: done rc={shell_rc} output_len={}",
        raw_output_str.len(),
    );

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
    for (key, uri_str, optional) in &prepared.bind_uris {
        let path = &prepared.bind_paths[key];
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
            bind_map: HashMap::from([("out?".to_string(), "artifact://out.txt".to_string())]),
        };
        let prepared = prepare_exec(&optional_exec, &registry, &registry, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.bind_uris[0].0, "out");
        assert!(prepared.bind_uris[0].2);

        // Non-optional bind: still a duplicate-producer error.
        let non_optional_exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
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
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
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
            bind_map: HashMap::from([("out?".to_string(), "artifact://out.txt".to_string())]),
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
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &registry, &registry, "", &fw)
            .await
            .unwrap();
        fs::write(&prepared.bind_paths["out"], "data").unwrap();
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
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
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
            bind_paths: HashMap::new(),
            bind_uris: Vec::new(),
            cmds: vec!["printf '%s' \"${GREMLINS_PR_TITLE}\"".to_string()],
            cwd: tmp.path().to_path_buf(),
            artifact_dir: artifact_dir.clone(),
            state_dir: state_dir.clone(),
            timeout: Some(5.0),
            env: HashMap::new(),
            substitution_env,
        };

        let result = run_shell(&prepared).await.unwrap();
        // The output must contain the literal payload — not execute it.
        assert_eq!(result.output, injection);
        // Neither marker file must exist.
        assert!(!Path::new("/tmp/gremlins_injection_test_marker").exists());
        assert!(!Path::new("/tmp/gremlins_injection_test_marker2").exists());
    }

    // --- run_shell header/footer framing ---

    fn make_prepared(name: &str, cmds: Vec<&str>, state_dir: &Path) -> ExecPrepared {
        ExecPrepared {
            name: name.to_string(),
            interpolation_map: HashMap::new(),
            bind_paths: HashMap::new(),
            bind_uris: Vec::new(),
            cmds: cmds.into_iter().map(|s| s.to_string()).collect(),
            cwd: std::env::current_dir().unwrap(),
            artifact_dir: state_dir.join("artifacts"),
            state_dir: state_dir.to_path_buf(),
            timeout: Some(5.0),
            env: HashMap::new(),
            substitution_env: HashMap::new(),
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
    async fn test_run_shell_writes_header_and_footer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("hello", vec!["echo UNIQUE_OUTPUT_MARKER"], &state_dir);
        let result = run_shell(&prepared).await.unwrap();
        assert_eq!(result.output, "UNIQUE_OUTPUT_MARKER");
        assert_eq!(result.rc, 0);

        let log = read_log(&state_dir, "hello");
        assert!(
            log.contains("=== exec stage: hello ==="),
            "missing header: {log}"
        );
        assert!(log.contains("cwd:"), "missing cwd: {log}");
        assert!(
            log.contains("command: echo UNIQUE_OUTPUT_MARKER"),
            "missing command: {log}"
        );
        assert!(
            log.contains("--- output ---"),
            "missing output marker: {log}"
        );
        assert!(
            log.contains("UNIQUE_OUTPUT_MARKER"),
            "missing command output: {log}"
        );
        assert!(log.contains("--- exit: 0"), "missing footer: {log}");
        assert!(log.contains("duration:"), "missing duration: {log}");

        // Verify ordering: header → output marker → command output → footer.
        // Use the second occurrence of UNIQUE_OUTPUT_MARKER (the actual output,
        // after the command line which also contains it).
        let header_pos = log.find("=== exec stage: hello ===").unwrap();
        let output_marker_pos = log.find("--- output ---").unwrap();
        let first_output = log.find("UNIQUE_OUTPUT_MARKER").unwrap();
        let second_output = log[first_output + 1..]
            .find("UNIQUE_OUTPUT_MARKER")
            .map(|p| p + first_output + 1);
        let world_pos = second_output.unwrap_or(first_output);
        let footer_pos = log.find("--- exit: 0").unwrap();
        assert!(header_pos < output_marker_pos);
        assert!(output_marker_pos < world_pos);
        assert!(world_pos < footer_pos);
    }

    #[tokio::test]
    async fn test_run_shell_header_footer_with_no_child_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("silent", vec!["true"], &state_dir);
        let result = run_shell(&prepared).await.unwrap();
        assert_eq!(result.output, "");
        assert_eq!(result.rc, 0);

        let log = read_log(&state_dir, "silent");
        assert!(
            log.contains("=== exec stage: silent ==="),
            "missing header: {log}"
        );
        assert!(log.contains("command: true"), "missing command: {log}");
        assert!(
            log.contains("--- output ---"),
            "missing output marker: {log}"
        );
        assert!(log.contains("--- exit: 0"), "missing footer: {log}");
    }

    #[tokio::test]
    async fn test_run_shell_footer_shows_nonzero_exit_code() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        let prepared = make_prepared("failing", vec!["exit 42"], &state_dir);

        let err = run_shell(&prepared).await.unwrap_err();
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
        run_shell(&prepared1).await.unwrap();
        run_shell(&prepared2).await.unwrap();

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
        let result = run_shell(&prepared).await.unwrap();
        assert_eq!(result.output, "still-works");
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
        let result = run_shell(&prepared).await.unwrap();
        assert_eq!(result.output, "ok");

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
