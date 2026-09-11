use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::core::proc::{run_shell_async, ProcError, ProcResult};
use crate::stages::base;
use crate::stages::constants::BAIL_KEY;

#[derive(Debug, Clone)]
pub struct Exec {
    pub name: String,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
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
    pub(crate) loop_iter: String,
}

/// Phase 1: resolve interpolation, compute bind paths, substitute commands.
/// Requires `&mut ArtifactRegistry` (for interpolation lookups). Returns a
/// fully-prepared struct that can be passed to `run_shell` and `commit_exec`
/// without further registry mutation.
pub fn prepare_exec(
    exec: &Exec,
    artifacts: &mut ArtifactRegistry,
    loop_iter: &str,
    framework_subs: &HashMap<String, String>,
) -> Result<ExecPrepared, ExecError> {
    let name = &exec.name;
    let str_opts = base::string_options(&exec.options);

    let interpolation_map =
        resolve_interpolation_map(artifacts, &exec.interpolation_map, loop_iter).map_err(|e| {
            ExecError::Resolve {
                name: name.clone(),
                source: e,
            }
        })?;

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
        // Liveness, not membership: a stale binding whose file was removed
        // (e.g. a skip_if_exists producer recovering) must not block the stage.
        if !optional && artifacts.is_live(&uri_str) {
            return Err(ExecError::Generic {
                name: name.clone(),
                detail: format!("artifact {uri_str:?} is already produced — duplicate producer"),
            });
        }
        let path = artifacts
            .path_for_uri(&uri)
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

    let cmds: Vec<String> = raw_cmds
        .iter()
        .map(|c| base::substitute_vars(c, &str_opts, &subst_vars, framework_subs))
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
        loop_iter: loop_iter.to_string(),
    })
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
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.insert(
        "GREMLINS_ARTIFACT_DIR".to_string(),
        prepared.artifact_dir.to_string_lossy().to_string(),
    );

    let result =
        run_shell_async(&joined, Some(&prepared.cwd), Some(&env), prepared.timeout).await?;

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

    let log_path = prepared.state_dir.join(format!("exec-{}.log", name));
    let log_content = if raw_output_str.is_empty() {
        "(no output)\n".to_string()
    } else {
        raw_output_str.clone()
    };
    if let Err(e) = std::fs::write(&log_path, &log_content) {
        log::warn!(
            "exec {name}: failed to write log to {}: {e}",
            log_path.display()
        );
    }

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

/// Phase 3: commit produced artifacts into the registry.
/// Non-optional artifacts that are absent abort the stage, except bail URIs.
pub fn commit_exec(
    prepared: &ExecPrepared,
    artifacts: &mut ArtifactRegistry,
) -> Result<(), ExecError> {
    for (key, uri_str, optional) in &prepared.bind_uris {
        let path = &prepared.bind_paths[key];
        if Path::new(path).exists() {
            artifacts
                .commit(uri_str, path)
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
    use std::fs;

    #[test]
    fn test_commit_exec_optional_bind_ignores_duplicate_registration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir);

        // A sibling already committed this URI.
        let uri = Uri::parse("artifact://out.txt").unwrap();
        registry.write_into_registry(&uri, "existing").unwrap();

        let fw = HashMap::new();

        // Optional bind: skipped, not an error.
        let optional_exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("out?".to_string(), "artifact://out.txt".to_string())]),
        };
        let prepared = prepare_exec(&optional_exec, &mut registry, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].0, "out");
        assert!(prepared.bind_uris[0].2);

        // Non-optional bind: still a duplicate-producer error.
        let non_optional_exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let err = prepare_exec(&non_optional_exec, &mut registry, "", &fw)
            .err()
            .expect("expected duplicate-producer error");
        assert!(matches!(err, ExecError::Generic { .. }));
    }

    #[test]
    fn test_prepare_exec_allows_recovering_from_stale_binding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir);

        // Registered but its file is gone: a skip_if_exists producer must be
        // able to run (and commit) again.
        let uri = Uri::parse("artifact://plan.md").unwrap();
        let stale = registry.write_into_registry(&uri, "# plan").unwrap();
        fs::remove_file(&stale).unwrap();

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("plan".to_string(), "artifact://plan.md".to_string())]),
        };
        let prepared = prepare_exec(&exec, &mut registry, "", &HashMap::new()).unwrap();
        fs::write(&prepared.bind_paths["plan"], "# new plan").unwrap();
        commit_exec(&prepared, &mut registry).unwrap();
        assert_eq!(
            registry.content("artifact://plan.md", None).unwrap(),
            "# new plan",
        );
    }

    #[test]
    fn test_commit_exec_missing_non_optional_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir);

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &mut registry, "", &fw).unwrap();
        let err = commit_exec(&prepared, &mut registry).unwrap_err();
        assert!(matches!(err, ExecError::MissingArtifact { .. }));
        assert!(!registry.is_registered("artifact://out.txt"));
    }

    #[test]
    fn test_commit_exec_allows_missing_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir);

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("out?".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &mut registry, "", &fw).unwrap();
        commit_exec(&prepared, &mut registry).unwrap();
        assert!(!registry.is_registered("artifact://out.txt"));
    }

    #[test]
    fn test_commit_exec_registers_produced_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir);

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("out".to_string(), "artifact://out.txt".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_exec(&exec, &mut registry, "", &fw).unwrap();
        fs::write(&prepared.bind_paths["out"], "data").unwrap();
        commit_exec(&prepared, &mut registry).unwrap();
        assert!(registry.is_registered("artifact://out.txt"));
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
}
