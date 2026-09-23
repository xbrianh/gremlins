use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use thiserror::Error;

use crate::artifacts::registry::{ArtifactRegistry, LocalizedArtifactRegistry};
use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::core::proc::{run_shell_async, ProcError, ProcResult};
use crate::stages::base;
use crate::stages::constants::{BAIL_KEY, FRAMEWORK_KEYS};

#[derive(Debug, Clone)]
pub struct Exec {
    pub name: String,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
}

impl Exec {
    /// Parse an `Exec` from a stage mapping.
    ///
    /// Mirrors `PyExec::with_dict`: the `in`/`out` rejection, the same mapping
    /// shapes, and the framework-key collision check — without the `model`
    /// exemption an agent gets, since an exec has no model. The `client` key is
    /// not read here; the client spec belongs to the stage-tree node.
    pub fn from_dict(d: &HashMap<String, serde_json::Value>) -> Result<Exec, String> {
        let name = d
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        if d.contains_key("in") || d.contains_key("out") {
            return Err(format!(
                "stage {name:?}: 'in'/'out' keys are no longer supported; \
                 use 'interpolation'/'bind' with URI values"
            ));
        }

        let interpolation_map = string_mapping(d, "interpolation", &name)?;
        let bind_map = string_mapping(d, "bind", &name)?;

        let options = match d.get("options") {
            None => HashMap::new(),
            Some(serde_json::Value::Object(options)) => options
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            Some(_) => {
                return Err(format!(
                    "stage {name:?}: 'options' must be a mapping of string keys to JSON-serializable values"
                ))
            }
        };

        for key in options.keys() {
            if FRAMEWORK_KEYS.contains(key.as_str()) {
                return Err(format!(
                    "stage {name:?}: option key {key:?} collides with framework substitution variable"
                ));
            }
        }

        Ok(Exec {
            name,
            options,
            interpolation_map,
            bind_map,
        })
    }
}

/// Read a string-to-string mapping field: absent is empty, and a present value
/// must be a mapping whose entries are all strings.
fn string_mapping(
    d: &HashMap<String, serde_json::Value>,
    field: &str,
    name: &str,
) -> Result<HashMap<String, String>, String> {
    match d.get(field) {
        None => Ok(HashMap::new()),
        Some(serde_json::Value::Object(entries)) => entries
            .iter()
            .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_string())))
            .collect::<Option<HashMap<String, String>>>()
            .ok_or_else(|| format!("stage {name:?}: '{field}' must be a mapping")),
        Some(_) => Err(format!("stage {name:?}: '{field}' must be a mapping")),
    }
}

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

pub fn is_bail_uri(uri_str: &str, loop_iter: &str) -> bool {
    if uri_str == BAIL_KEY {
        return true;
    }
    if loop_iter.is_empty() {
        return false;
    }
    let expected = format!("artifact://{loop_iter}/bail");
    uri_str == expected || uri_str == "artifact://{loop_iter}/bail"
}

// --- Phased execution model ---

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
    pub(crate) loop_iter: String,
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
    let str_opts = base::string_options(&exec.options);

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
        let k = base::substitute_vars(raw_key, &str_opts, &interpolation_map, framework_subs);
        let optional = k.ends_with('?');
        let key = k.trim_end_matches('?').to_string();
        let mut uri_str =
            base::substitute_vars(raw_uri_str, &str_opts, &interpolation_map, framework_subs);
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
            base::substitute_vars_to_env(
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
        loop_iter: loop_iter.to_string(),
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
        let _ = std::fs::write(&stream_path, header);
    }

    let start = std::time::Instant::now();
    let result = run_shell_async(
        &joined,
        Some(&prepared.cwd),
        Some(&env),
        prepared.timeout,
        stream_path_arg,
    )
    .await?;
    let elapsed = start.elapsed();

    // Append footer to stream file (best-effort).
    if stream_path_arg.is_some() {
        let footer = format!(
            "\n--- exit: {} (duration: {:.1}s) ---\n",
            result.returncode,
            elapsed.as_secs_f64()
        );
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .open(&stream_path)
            .and_then(|mut f| f.write_all(footer.as_bytes()));
    }

    process_shell_result(prepared, result)
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

    // A non-zero exit is an error unless a bind URI is a bail URI; in that
    // case the failure is reported by the caller as a Python exception.
    if shell_rc != 0
        && !prepared
            .bind_uris
            .iter()
            .any(|(_, uri_str, _)| is_bail_uri(uri_str, &prepared.loop_iter))
    {
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
        } else if !*optional && !is_bail_uri(uri_str, &prepared.loop_iter) {
            return Err(ExecError::MissingArtifact {
                name: prepared.name.clone(),
                uri: uri_str.clone(),
            });
        }
    }
    Ok(())
}

impl crate::stages::base::Stage for Exec {
    fn name(&self) -> &str {
        &self.name
    }

    fn stage_type(&self) -> &str {
        "exec"
    }

    fn path(&self) -> &str {
        ""
    }

    fn set_path(&mut self, _path: &str) {}

    fn client(&self) -> Option<&str> {
        None
    }

    fn set_client(&mut self, _client: Option<String>) {}

    fn client_explicit(&self) -> bool {
        false
    }

    fn set_client_explicit(&mut self, _explicit: bool) {}

    fn body(&self) -> &[Box<dyn crate::stages::base::Stage>] {
        &[]
    }

    fn bind_map(&self) -> &HashMap<String, String> {
        &self.bind_map
    }

    fn options(&self) -> &HashMap<String, serde_json::Value> {
        &self.options
    }

    fn skip_if_exists(&self) -> &str {
        ""
    }

    fn set_skip_if_exists(&mut self, _skip: String) {}
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

    #[test]
    fn test_exec_implements_stage_trait() {
        let exec = Exec {
            name: "test-exec".to_string(),
            options: HashMap::from([(
                "greeting".to_string(),
                serde_json::Value::String("hi".to_string()),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("key".to_string(), "value".to_string())]),
        };

        let stage: &dyn crate::stages::base::Stage = &exec;
        assert_eq!(stage.name(), "test-exec");
        assert_eq!(stage.stage_type(), "exec");
        assert_eq!(stage.path(), "");
        assert!(stage.client().is_none());
        assert!(!stage.client_explicit());
        assert!(stage.body().is_empty());
        assert_eq!(stage.bind_map().get("key").unwrap(), "value");
        assert_eq!(
            stage.options().get("greeting").unwrap(),
            &serde_json::Value::String("hi".to_string())
        );
        assert_eq!(stage.skip_if_exists(), "");

        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = stage.substitute_vars("{greeting}", &extra, &fw);
        assert_eq!(result, "hi");
    }
    #[test]
    fn test_is_bail_uri_bail_key() {
        assert!(is_bail_uri(BAIL_KEY, ""));
        assert!(is_bail_uri(BAIL_KEY, "loop~1"));
    }

    #[test]
    fn test_is_bail_uri_with_loop_iter() {
        assert!(is_bail_uri("artifact://loop~1/bail", "loop~1"));
        assert!(!is_bail_uri("artifact://loop~1/bail", ""));
    }

    #[test]
    fn test_is_bail_uri_template() {
        assert!(is_bail_uri("artifact://{loop_iter}/bail", "loop~1"));
        assert!(!is_bail_uri("artifact://stuff/bail", "loop~1"));
    }

    #[test]
    fn test_is_bail_uri_no_match() {
        assert!(!is_bail_uri("artifact://stuff", ""));
        assert!(!is_bail_uri("artifact://stuff", "loop~1"));
    }

    // ---- from_dict tests ----

    fn exec_dict(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn from_dict_parses_options_and_maps() {
        let d = exec_dict(&[
            ("name", serde_json::json!("run")),
            ("options", serde_json::json!({"cmds": ["echo hi"]})),
            (
                "interpolation",
                serde_json::json!({"a": "content(\"artifact://a\")"}),
            ),
            ("bind", serde_json::json!({"b": "artifact://b"})),
        ]);
        let exec = Exec::from_dict(&d).unwrap();
        assert_eq!(exec.name, "run");
        assert_eq!(
            exec.options.get("cmds").unwrap(),
            &serde_json::json!(["echo hi"])
        );
        assert_eq!(
            exec.interpolation_map.get("a").unwrap(),
            "content(\"artifact://a\")"
        );
        assert_eq!(exec.bind_map.get("b").unwrap(), "artifact://b");
    }

    #[test]
    fn from_dict_defaults_are_empty() {
        let exec = Exec::from_dict(&exec_dict(&[])).unwrap();
        assert_eq!(exec.name, "");
        assert!(exec.options.is_empty());
        assert!(exec.interpolation_map.is_empty());
        assert!(exec.bind_map.is_empty());
    }

    #[test]
    fn from_dict_rejects_in_and_out() {
        for key in ["in", "out"] {
            let d = exec_dict(&[
                ("name", serde_json::json!("s")),
                (key, serde_json::json!({})),
            ]);
            let err = Exec::from_dict(&d).unwrap_err();
            assert!(
                err.contains("'in'/'out' keys are no longer supported"),
                "{err}"
            );
        }
    }

    #[test]
    fn from_dict_rejects_non_mapping_bind() {
        let d = exec_dict(&[
            ("name", serde_json::json!("s")),
            ("bind", serde_json::json!("not a mapping")),
        ]);
        let err = Exec::from_dict(&d).unwrap_err();
        assert!(err.contains("'bind' must be a mapping"), "{err}");
    }

    #[test]
    fn from_dict_rejects_null_options() {
        let d = exec_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::Value::Null),
        ]);
        assert!(Exec::from_dict(&d).is_err());
    }

    #[test]
    fn from_dict_rejects_every_framework_option_key_including_model() {
        // Unlike an agent, an exec rejects `model` too.
        for key in ["name", "model", "cwd", "base_ref"] {
            let d = exec_dict(&[
                ("name", serde_json::json!("s")),
                ("options", serde_json::json!({ key: "x" })),
            ]);
            let err = Exec::from_dict(&d).unwrap_err();
            assert!(
                err.contains("collides with framework substitution variable"),
                "{key}: {err}"
            );
        }
    }

    #[test]
    fn from_dict_rejects_non_object_options() {
        let d = exec_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::json!([1, 2, 3])),
        ]);
        assert!(Exec::from_dict(&d).is_err());
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
            loop_iter: String::new(),
            substitution_env,
        };

        let result = run_shell(&prepared).await.unwrap();
        // The output must contain the literal payload — not execute it.
        assert_eq!(result.output, injection);
        // Neither marker file must exist.
        assert!(!Path::new("/tmp/gremlins_injection_test_marker").exists());
        assert!(!Path::new("/tmp/gremlins_injection_test_marker2").exists());
    }
}
