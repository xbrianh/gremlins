use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use thiserror::Error;

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::resolve::resolve_interpolation_map;
use crate::artifacts::uri::Uri;
use crate::core::proc::{run_shell_async, ProcError, ProcResult};
use crate::stages::constants::BAIL_KEY;

static VAR_SUB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([-\w]+)\}").unwrap());

#[derive(Debug, Clone)]
pub struct Exec {
    pub name: String,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
}

#[derive(Error, Debug)]
pub enum ExecError {
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

/// Substitute `{var}` tokens in `text` using the same resolution order:
/// string options → extra (bind/interpolation) → framework_subs.
/// Framework subs win on collision. Hyphen-normalized variants are added
/// for underscore keys.
pub fn substitute_vars(
    text: &str,
    string_options: &HashMap<String, String>,
    extra: &HashMap<String, String>,
    framework_subs: &HashMap<String, String>,
) -> String {
    let mut subs: HashMap<String, String> = HashMap::new();
    subs.extend(string_options.iter().map(|(k, v)| (k.clone(), v.clone())));
    subs.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    subs.extend(framework_subs.iter().map(|(k, v)| (k.clone(), v.clone())));

    // Add hyphen-normalized variants for underscore keys
    let hyphenated: Vec<(String, String)> = subs
        .iter()
        .filter_map(|(k, v)| {
            if k.contains('_') {
                Some((k.replace('_', "-"), v.clone()))
            } else {
                None
            }
        })
        .collect();
    for (hk, hv) in hyphenated {
        subs.entry(hk).or_insert(hv);
    }

    VAR_SUB_RE
        .replace_all(text, |caps: &regex::Captures| {
            // Check if preceded by '$' (manual lookbehind since Rust regex doesn't support it)
            let start = caps.get(0).unwrap().start();
            if start > 0 && text.as_bytes()[start - 1] == b'$' {
                return caps.get(0).unwrap().as_str().to_string();
            }
            let key = caps.get(1).unwrap().as_str();
            if let Some(val) = subs.get(key) {
                return val.clone();
            }
            let alt = key.replace('-', "_");
            if let Some(val) = subs.get(&alt) {
                return val.clone();
            }
            caps.get(0).unwrap().as_str().to_string()
        })
        .to_string()
}

fn string_options(
    options: &HashMap<String, serde_json::Value>,
) -> HashMap<String, String> {
    options
        .iter()
        .filter_map(|(k, v)| {
            if let serde_json::Value::String(s) = v {
                Some((k.clone(), s.clone()))
            } else {
                None
            }
        })
        .collect()
}

pub async fn run_exec_stage(
    exec: &Exec,
    artifacts: &mut ArtifactRegistry,
    loop_iter: &str,
    cwd: &Path,
    artifact_dir: &Path,
    state_dir: &Path,
    framework_subs: &HashMap<String, String>,
) -> Result<ProcResult, ExecError> {
    let name = &exec.name;
    let str_opts = string_options(&exec.options);

    // Resolve interpolation vars
    let interpolation_map =
        resolve_interpolation_map(artifacts, &exec.interpolation_map, loop_iter)
            .map_err(|e| ExecError::Generic {
                name: name.clone(),
                detail: e.to_string(),
            })?;

    // Register bind URIs and collect output paths
    let mut bind_paths: HashMap<String, String> = HashMap::new();
    for (raw_key, raw_uri_str) in &exec.bind_map {
        let k = substitute_vars(raw_key, &str_opts, &interpolation_map, framework_subs);
        let optional = k.ends_with('?');
        let key = k.trim_end_matches('?').to_string();
        let mut uri_str =
            substitute_vars(raw_uri_str, &str_opts, &interpolation_map, framework_subs);
        if !loop_iter.is_empty() {
            uri_str = uri_str.replace("{loop_iter}", loop_iter);
        }
        let uri = Uri::parse(&uri_str).map_err(|e| ExecError::Generic {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        let path = artifacts.register(&uri, true).map_err(|e| ExecError::Generic {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        if optional {
            bind_paths.insert(format!("{key}?"), path);
        } else {
            bind_paths.insert(key, path);
        }
    }

    // Merge interpolation_map and bind_paths (bind shadows interpolation)
    let subst_vars: HashMap<String, String> = interpolation_map
        .iter()
        .chain(bind_paths.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Read cmds, filter empties, substitute vars
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
        .map(|c| substitute_vars(c, &str_opts, &subst_vars, framework_subs))
        .collect();

    let timeout: Option<f64> = exec.options.get("timeout").and_then(|v| v.as_f64());

    log::info!(
        "exec {name}: entering stage, {} command(s), timeout={}, cwd={}",
        cmds.len(),
        timeout
            .map(|t| format!("{t}s"))
            .unwrap_or_else(|| "none".to_string()),
        cwd.display(),
    );

    let (shell_output, shell_rc, bail_triggered) = if cmds.is_empty() {
        (String::new(), 0, false)
    } else {
        let joined = cmds.join(" && ");
        let mut env = HashMap::new();
        env.insert(
            "GREMLINS_ARTIFACT_DIR".to_string(),
            artifact_dir.to_string_lossy().to_string(),
        );

        let result = run_shell_async(&joined, Some(cwd), Some(&env), timeout).await?;

        let raw_output = {
            let mut buf = result.stdout.clone();
            buf.extend_from_slice(&result.stderr);
            buf
        };
        let raw_output_str = String::from_utf8_lossy(&raw_output).to_string();
        let shell_output = raw_output_str.trim().to_string();
        let shell_rc = result.returncode;

        // Write log
        let log_path = state_dir.join(format!("exec-{name}.log"));
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

        let bail_triggered = if shell_rc != 0 {
            if exec.bind_map.values().any(|v| is_bail_uri(v, loop_iter)) {
                true
            } else {
                return Err(ExecError::NonZeroExit {
                    name: name.clone(),
                    rc: shell_rc,
                    output: Some(shell_output.clone()),
                });
            }
        } else {
            false
        };

        (shell_output, shell_rc, bail_triggered)
    };

    // Post-command verification
    for (raw_key, raw_uri_str) in &exec.bind_map {
        let k = substitute_vars(raw_key, &str_opts, &interpolation_map, framework_subs);
        let optional = k.ends_with('?');
        let _key = k.trim_end_matches('?').to_string();
        let mut uri_str =
            substitute_vars(raw_uri_str, &str_opts, &interpolation_map, framework_subs);
        if !loop_iter.is_empty() {
            uri_str = uri_str.replace("{loop_iter}", loop_iter);
        }
        if is_bail_uri(&uri_str, loop_iter) && !bail_triggered {
            continue;
        }
        if !artifacts.exists(&uri_str) {
            if optional {
                continue;
            }
            return Err(ExecError::MissingArtifact {
                name: name.clone(),
                uri: uri_str,
            });
        }
    }

    Ok(ProcResult {
        returncode: shell_rc,
        stdout: shell_output.into_bytes(),
        stderr: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

    #[test]
    fn test_substitute_vars_basic() {
        let opts = HashMap::new();
        let extra = HashMap::from([("var".to_string(), "world".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("hello {var}", &opts, &extra, &fw);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_substitute_vars_hyphen_normalization() {
        let opts = HashMap::new();
        let extra = HashMap::from([("child_plan".to_string(), "value".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("{child-plan}", &opts, &extra, &fw);
        assert_eq!(result, "value");
    }

    #[test]
    fn test_substitute_vars_framework_overrides() {
        let opts = HashMap::new();
        let extra = HashMap::from([("name".to_string(), "extra".to_string())]);
        let fw = HashMap::from([("name".to_string(), "fw".to_string())]);
        let result = substitute_vars("{name}", &opts, &extra, &fw);
        assert_eq!(result, "fw");
    }

    #[test]
    fn test_substitute_vars_unknown_token() {
        let opts = HashMap::new();
        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = substitute_vars("hello {unknown}", &opts, &extra, &fw);
        assert_eq!(result, "hello {unknown}");
    }

    #[test]
    fn test_substitute_vars_escaped_brace() {
        let opts = HashMap::new();
        let extra = HashMap::from([("x".to_string(), "y".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("\\${x}", &opts, &extra, &fw);
        assert_eq!(result, "\\${x}");
    }

    #[test]
    fn test_substitute_vars_string_opts() {
        // String options have lowest priority
        let opts = HashMap::from([("foo".to_string(), "opt".to_string())]);
        let extra = HashMap::from([("foo".to_string(), "extra".to_string())]);
        let fw = HashMap::new();
        let result = substitute_vars("{foo}", &opts, &extra, &fw);
        // Extra shadows opts
        assert_eq!(result, "extra");
    }

    #[tokio::test]
    async fn test_run_exec_stage_trivial() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir.clone());

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::from([(
                "cmds".to_string(),
                serde_json::json!(["echo hello"]),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };

        let fw = HashMap::new();
        let result = run_exec_stage(
            &exec,
            &mut registry,
            "",
            tmp.path(),
            &artifact_dir,
            &state_dir,
            &fw,
        )
        .await
        .unwrap();
        assert_eq!(result.returncode, 0);
        assert!(String::from_utf8_lossy(&result.stdout).contains("hello"));
    }

    #[tokio::test]
    async fn test_run_exec_stage_timeout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir.clone());

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::from([
                (
                    "cmds".to_string(),
                    serde_json::json!(["sleep 10"]),
                ),
                ("timeout".to_string(), serde_json::json!(0.05)),
            ]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };

        let fw = HashMap::new();
        let err = run_exec_stage(
            &exec,
            &mut registry,
            "",
            tmp.path(),
            &artifact_dir,
            &state_dir,
            &fw,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::Proc(ProcError::TimeoutExpired(..))));
    }

    #[tokio::test]
    async fn test_run_exec_stage_bail_on_exit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&artifact_dir).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        let mut registry = ArtifactRegistry::new(artifact_dir.clone());

        let bail_file = artifact_dir.join("bail");
        fs::write(&bail_file, "bail data").unwrap();
        registry
            .register(&Uri::parse("artifact://bail").unwrap(), true)
            .unwrap();

        let exec = Exec {
            name: "test".to_string(),
            options: HashMap::from([(
                "cmds".to_string(),
                serde_json::json!(["exit 2"]),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("bail".to_string(), BAIL_KEY.to_string())]),
        };

        let fw = HashMap::new();
        let result = run_exec_stage(
            &exec,
            &mut registry,
            "",
            tmp.path(),
            &artifact_dir,
            &state_dir,
            &fw,
        )
        .await
        .unwrap();
        assert_eq!(result.returncode, 2);
    }
}