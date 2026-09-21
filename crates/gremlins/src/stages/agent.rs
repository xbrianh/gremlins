use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::clients::protocol::CompletedRun;
use crate::stages::base;
use crate::stages::constants::FRAMEWORK_KEYS;

// ---------------------------------------------------------------------------
// Agent struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub prompts: Vec<String>,
    pub options: HashMap<String, serde_json::Value>,
    pub interpolation_map: HashMap<String, String>,
    pub bind_map: HashMap<String, String>,
}

impl Agent {
    /// Parse an `Agent` from a stage mapping.
    ///
    /// Mirrors `PyAgent::with_dict` field for field — the `in`/`out`
    /// rejection, the framework-key collision check (`model` excepted, since an
    /// agent may target one), and the requirement that `prompt` be a list of
    /// strings. The `client` key is deliberately not read here: the client spec
    /// is a definition concern and lives on the stage-tree node.
    pub fn from_dict(d: &HashMap<String, serde_json::Value>) -> Result<Agent, String> {
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
            if FRAMEWORK_KEYS.contains(key.as_str()) && key != "model" {
                return Err(format!(
                    "stage {name:?}: option key {key:?} collides with framework substitution variable"
                ));
            }
        }

        let prompts = match d.get("prompt") {
            None => Vec::new(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(String::from).ok_or_else(|| {
                        format!("stage {name:?}: 'prompt' must be a list of strings")
                    })
                })
                .collect::<Result<Vec<String>, String>>()?,
            Some(_) => {
                return Err(format!(
                    "stage {name:?}: 'prompt' must be a list of strings"
                ))
            }
        };

        Ok(Agent {
            name,
            prompts,
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

// ---------------------------------------------------------------------------
// AgentPrepared — fully resolved pre-run state
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct AgentPrepared {
    pub name: String,
    pub prompt: String,
    pub model: Option<String>,
    pub(crate) bind_paths: HashMap<String, String>,
    /// (key, uri_str, optional)
    pub(crate) bind_uris: Vec<(String, String, bool)>,
    pub expected_artifact_paths: Vec<String>,
    pub cwd: String,
    pub worktree: Option<String>,
    pub artifact_dir: String,
}

// ---------------------------------------------------------------------------
// AgentError
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("agent {name}: {reason}")]
    Bail { name: String, reason: String },
    #[error("agent {name}: artifact {key} was not produced")]
    MissingArtifact { name: String, key: String },
    #[error("agent {name}: {source}")]
    Resolve {
        name: String,
        #[source]
        source: ResolveError,
    },
    #[error("agent {name}: {detail}")]
    Generic { name: String, detail: String },
}

impl From<ResolveError> for AgentError {
    fn from(source: ResolveError) -> Self {
        AgentError::Resolve {
            name: String::new(),
            source,
        }
    }
}

// ---------------------------------------------------------------------------
// prepare_agent — phase 1: resolve everything without touching the client
// ---------------------------------------------------------------------------

pub fn prepare_agent(
    agent: &Agent,
    artifacts: &ArtifactRegistry,
    loop_iter: &str,
    framework_subs: &HashMap<String, String>,
) -> Result<AgentPrepared, AgentError> {
    let name = &agent.name;
    let str_opts = base::string_options(&agent.options);

    let interpolation_map =
        resolve_interpolation_map(artifacts, &agent.interpolation_map, loop_iter).map_err(|e| {
            AgentError::Resolve {
                name: name.clone(),
                source: e,
            }
        })?;

    let mut bind_paths: HashMap<String, String> = HashMap::new();
    let mut bind_opaque_keys: HashMap<String, String> = HashMap::new();
    let mut bind_uris: Vec<(String, String, bool)> = Vec::new();
    for (raw_key, raw_uri_str) in &agent.bind_map {
        let k = base::substitute_vars(raw_key, &str_opts, &interpolation_map, framework_subs);
        let optional = k.ends_with('?');
        let key = k.trim_end_matches('?').to_string();
        let mut uri_str =
            base::substitute_vars(raw_uri_str, &str_opts, &interpolation_map, framework_subs);
        if !loop_iter.is_empty() {
            uri_str = uri_str.replace("{loop_iter}", loop_iter);
        }
        let uri = Uri::parse(&uri_str).map_err(|e| AgentError::Generic {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        // Optional binds are skipped when a sibling already committed the URI.
        // Liveness, not membership: a stale binding whose file was removed
        // (e.g. a skip_if_exists producer recovering) must not block the stage.
        if !optional && artifacts.is_live(&uri_str) {
            return Err(AgentError::Generic {
                name: name.clone(),
                detail: format!("artifact {uri_str:?} is already produced — duplicate producer"),
            });
        }
        // Use opaque_path for agent-facing keys; path_for_uri for real paths.
        let (opaque_key, real_path) =
            artifacts
                .opaque_path(&uri)
                .map_err(|e| AgentError::Generic {
                    name: name.clone(),
                    detail: e.to_string(),
                })?;
        bind_paths.insert(key.clone(), real_path);
        bind_opaque_keys.insert(key.clone(), opaque_key);
        bind_uris.push((key, uri_str, optional));
    }

    // Merge: bind opaque keys shadow interpolation keys on collision
    let subst_vars: HashMap<String, String> = interpolation_map
        .iter()
        .chain(bind_opaque_keys.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let template = agent.prompts.join("\n\n").trim_end().to_string();
    let prompt = base::substitute_vars(&template, &str_opts, &subst_vars, framework_subs);

    // Model substitution
    let model = agent
        .options
        .get("model")
        .and_then(|v| v.as_str())
        .map(|raw| base::substitute_vars(raw, &str_opts, &subst_vars, framework_subs));

    let expected_artifact_paths: Vec<String> = bind_paths.values().cloned().collect();

    Ok(AgentPrepared {
        name: name.clone(),
        prompt,
        model,
        bind_paths,
        bind_uris,
        expected_artifact_paths,
        cwd: String::new(),
        worktree: None,
        artifact_dir: String::new(),
    })
}

// ---------------------------------------------------------------------------
// commit_agent
// ---------------------------------------------------------------------------

/// Commit produced artifacts into the registry. Every non-optional bind must
/// have a non-empty produced file; only produced files are committed. Optional
/// binds may be absent.
pub fn commit_agent(
    prepared: &AgentPrepared,
    artifacts: &ArtifactRegistry,
) -> Result<(), AgentError> {
    for (key, uri_str, optional) in &prepared.bind_uris {
        let path = &prepared.bind_paths[key];
        let produced = std::path::Path::new(path)
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0)
            > 0;
        if produced {
            artifacts
                .commit(uri_str, path)
                .map_err(|e| AgentError::Generic {
                    name: prepared.name.clone(),
                    detail: e.to_string(),
                })?;
        } else if !*optional {
            return Err(AgentError::MissingArtifact {
                name: prepared.name.clone(),
                key: key.clone(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// check_bail
// ---------------------------------------------------------------------------

pub fn check_bail(completed: &CompletedRun) -> Result<(), AgentError> {
    let text = completed.text_result.as_deref().unwrap_or("");
    let last_line = text
        .lines()
        .rev()
        .find(|ln| !ln.trim().is_empty())
        .unwrap_or("");
    // Format must match: BAIL: <class>: <reason>
    let trimmed = last_line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("BAIL:") {
        let rest = rest.trim_start();
        if let Some((_class, reason)) = rest.split_once(':') {
            let reason = reason.trim().to_string();
            return Err(AgentError::Bail {
                name: String::new(),
                reason,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workspace preamble assembly
// ---------------------------------------------------------------------------

pub(crate) fn build_workspace_preamble(cwd: &str, worktree: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !cwd.is_empty() {
        parts.push(format!("Your working directory is: {cwd}"));
    }
    if let Some(wt) = worktree {
        if !wt.is_empty() && wt != cwd {
            parts.push(format!("Project worktree: {wt}"));
        }
    }
    parts.push(
        "Relevant environment variables: $GREMLINS_WORKTREE_PATH, $GREMLIN_WORKSPACE_DIR"
            .to_string(),
    );
    parts.push(
        "Artifact files are accessed via opaque hex keys (e.g. a1b2c3d4e5f.md) \
         provided in the prompt — pass them directly as file_path to Read/Write tools."
            .to_string(),
    );
    parts.join("\n")
}

impl AgentPrepared {
    /// The harness system prompt — the full output of agent_system_prompt().
    pub fn system_prompt(&self) -> String {
        let scratch = Path::new(&self.artifact_dir);
        let cwd = Path::new(&self.cwd);
        crate::clients::config::agent_system_prompt(cwd, scratch)
    }

    /// Workspace preamble + stage prompt (no harness system content).
    pub fn user_prompt(&self) -> String {
        let preamble = build_workspace_preamble(&self.cwd, self.worktree.as_deref());
        format!("{preamble}\n\n{}", self.prompt)
    }
}

// ---------------------------------------------------------------------------
// Stage trait impl
// ---------------------------------------------------------------------------

impl crate::stages::base::Stage for Agent {
    fn name(&self) -> &str {
        &self.name
    }

    fn stage_type(&self) -> &str {
        "agent"
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::artifacts::registry::ArtifactRegistry;

    // ---- check_bail tests ----

    #[test]
    fn test_check_bail_finds_bail() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("BAIL: security: found secret".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        let err = check_bail(&cr).unwrap_err();
        match err {
            AgentError::Bail { reason, .. } => assert_eq!(reason, "found secret"),
            _ => panic!("expected Bail"),
        }
    }

    #[test]
    fn test_check_bail_no_bail() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("All good here".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        assert!(check_bail(&cr).is_ok());
    }

    #[test]
    fn test_check_bail_empty() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some(String::new()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        assert!(check_bail(&cr).is_ok());
    }

    #[test]
    fn test_check_bail_blank() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("\n\n  \n".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        assert!(check_bail(&cr).is_ok());
    }

    #[test]
    fn test_check_bail_last_line_only() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("work done\nBAIL: other: timed out".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        let err = check_bail(&cr).unwrap_err();
        match err {
            AgentError::Bail { reason, .. } => assert_eq!(reason, "timed out"),
            _ => panic!("expected Bail"),
        }
    }

    #[test]
    fn test_check_bail_not_last_line() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("BAIL: other: early\nwork done".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        assert!(check_bail(&cr).is_ok());
    }

    #[test]
    fn test_check_bail_empty_reason() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("BAIL: other: ".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        let err = check_bail(&cr).unwrap_err();
        match err {
            AgentError::Bail { reason, .. } => assert_eq!(reason, ""),
            _ => panic!("expected Bail"),
        }
    }

    #[test]
    fn test_check_bail_no_bail_class() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("BAIL:".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        assert!(check_bail(&cr).is_ok());
    }

    #[test]
    fn test_check_bail_requires_class() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("BAIL: other".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        assert!(check_bail(&cr).is_ok());
    }

    #[test]
    fn test_check_bail_whitespace_edges() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("BAIL:  other  :  spaced  ".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        let err = check_bail(&cr).unwrap_err();
        match err {
            AgentError::Bail { reason, .. } => assert_eq!(reason, "spaced"),
            _ => panic!("expected Bail"),
        }
    }

    #[test]
    fn test_check_bail_multiline_skips_trailing_blanks() {
        let cr = CompletedRun {
            exit_code: 0,
            text_result: Some("work\nBAIL: sec: found\n\n".to_string()),
            events: None,
            cost_usd: None,
            token_usage: None,
        };
        let err = check_bail(&cr).unwrap_err();
        match err {
            AgentError::Bail { reason, .. } => assert_eq!(reason, "found"),
            _ => panic!("expected Bail"),
        }
    }

    // ---- prepare_agent tests ----

    fn make_registry(artifact_dir: PathBuf) -> ArtifactRegistry {
        ArtifactRegistry::new(artifact_dir)
    }

    fn register_file(reg: &ArtifactRegistry, name: &str, content: &str) -> String {
        let uri = Uri::parse(&format!("artifact://{name}")).unwrap();
        reg.write_into_registry(&uri, content).unwrap()
    }

    fn ensure_artifact_dir(tmp: &tempfile::TempDir) -> PathBuf {
        let ad = tmp.path().join("artifacts");
        std::fs::create_dir_all(&ad).unwrap();
        ad
    }

    #[test]
    fn test_prepare_basic_prompt_substitution() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        register_file(&reg, "world", "world-value");
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Hello {var}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "var".to_string(),
                r#"content("artifact://world")"#.to_string(),
            )]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert!(prepared.prompt.contains("world-value"));
    }

    #[test]
    fn test_prepare_framework_subs_win() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        register_file(&reg, "interp-src", "from-interp");
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Hello {name}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "name".to_string(),
                r#"content("artifact://interp-src")"#.to_string(),
            )]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::from([("name".to_string(), "from-fw".to_string())]);
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert!(prepared.prompt.contains("from-fw"));
        assert!(!prepared.prompt.contains("from-interp"));
    }

    #[test]
    fn test_prepare_bind_shadows_interpolation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad.clone());
        register_file(&reg, "interp-val", "interp-val");
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{key}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "key".to_string(),
                r#"content("artifact://interp-val")"#.to_string(),
            )]),
            bind_map: HashMap::from([("key".to_string(), "file://session/out.md".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        // bind_paths should contain the real path (for commit_agent), not "interp-val"
        assert!(prepared.bind_paths.contains_key("key"));
        let path = &prepared.bind_paths["key"];
        // The real path is artifact_dir/<hex>.md, not artifact_dir/out.md
        assert!(path.ends_with(".md"));
        assert!(path.starts_with(reg.artifact_dir.to_string_lossy().as_ref()));
        // The prompt should use the opaque hex key (shadow), not the real path
        assert!(!prepared.prompt.contains("out.md"));
        assert!(!prepared.prompt.contains("interp-val"));
        // The prompt contains an 11-char hex key with .md extension
        let prompt_words: Vec<&str> = prepared.prompt.split_whitespace().collect();
        let hex_word = prompt_words.iter().find(|w| w.ends_with(".md")).unwrap();
        assert_eq!(hex_word.len(), 14); // 11 hex + ".md"
        let dot = hex_word.find('.').unwrap();
        assert!(hex_word[..dot].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_prepare_missing_interpolation_key_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{missing}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([("missing".to_string(), "nonexistent".to_string())]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let err = prepare_agent(&agent, &reg, "", &fw).unwrap_err();
        assert!(matches!(err, AgentError::Resolve { .. }));
    }

    #[test]
    fn test_prepare_loop_iter_in_bind_uri() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{out}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([(
                "out".to_string(),
                "artifact://{loop_iter}/out.txt".to_string(),
            )]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "my-agent~3", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].1, "artifact://my-agent~3/out.txt");
    }

    #[test]
    fn test_prepare_loop_iter_in_interpolation_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad.clone());
        // Pre-register the artifact that content() will look up
        let plan_uri = Uri::parse("artifact://my-agent~2/plan.md").unwrap();
        reg.write_into_registry(&plan_uri, "# Plan").unwrap();
        // Register bind for the output so verify doesn't fail
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Plan: {plan}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "plan".to_string(),
                r#"content("artifact://{loop_iter}/plan.md")"#.to_string(),
            )]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "my-agent~2", &fw).unwrap();
        assert!(prepared.prompt.contains("Plan: # Plan"));
    }

    #[test]
    fn test_prepare_optional_bind_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{result}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("result?".to_string(), "file://session/out.md".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].0, "result");
        assert!(prepared.bind_uris[0].2); // optional
    }

    #[test]
    fn test_prepare_model_substituted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        register_file(&reg, "openai", "openai");
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::from([(
                "model".to_string(),
                serde_json::Value::String("{provider}:{variant}".to_string()),
            )]),
            interpolation_map: HashMap::from([(
                "provider".to_string(),
                r#"content("artifact://openai")"#.to_string(),
            )]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert_eq!(prepared.model, Some("openai:{variant}".to_string()));
    }

    #[test]
    fn test_prepare_model_none_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert!(prepared.model.is_none());
    }

    #[test]
    fn test_prepare_workspace_preamble_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        prepared.cwd = String::new();
        prepared.worktree = None;
        let preamble = build_workspace_preamble(&prepared.cwd, prepared.worktree.as_deref());
        assert!(preamble.contains(
            "Relevant environment variables: $GREMLINS_WORKTREE_PATH, $GREMLIN_WORKSPACE_DIR"
        ));
        assert!(preamble.contains("opaque hex keys"));
        let full = format!("{preamble}\n\n{}", prepared.prompt);
        assert!(!full.contains("Your working directory is"));
    }

    #[test]
    fn test_prepare_workspace_preamble_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        prepared.cwd = "/work".to_string();
        prepared.worktree = Some("/work".to_string());
        let preamble = build_workspace_preamble(&prepared.cwd, prepared.worktree.as_deref());
        let full = format!("{preamble}\n\n{}", prepared.prompt);
        assert!(full.contains("Your working directory is: /work"));
        assert!(!full.contains("Project worktree:"));
    }

    #[test]
    fn test_prepare_workspace_preamble_different_worktree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        prepared.cwd = "/tmp/run".to_string();
        prepared.worktree = Some("/repo".to_string());
        let preamble = build_workspace_preamble(&prepared.cwd, prepared.worktree.as_deref());
        let full = format!("{preamble}\n\n{}", prepared.prompt);
        assert!(full.contains("Your working directory is: /tmp/run"));
        assert!(full.contains("Project worktree: /repo"));
    }

    #[test]
    fn test_prepare_name_substitution_in_bind_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{my-agent}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([(
                "{name}".to_string(),
                "file://session/{name}.md".to_string(),
            )]),
        };
        let fw = HashMap::from([("name".to_string(), "my-agent".to_string())]);
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].0, "my-agent");
        assert!(prepared.bind_uris[0].1.contains("my-agent.md"));
    }

    #[test]
    fn test_prepare_prompts_joined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec![
                "Line 1".to_string(),
                "Line 2".to_string(),
                "Line 3".to_string(),
            ],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert_eq!(prepared.prompt, "Line 1\n\nLine 2\n\nLine 3");
    }

    #[test]
    fn test_prepare_hyphen_normalization_in_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        register_file(&reg, "value", "value");
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{child-plan}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "child_plan".to_string(),
                "artifact://value".to_string(),
            )]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert!(prepared.prompt.contains("value"));
    }

    #[test]
    fn test_prepare_options_string_filtering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let mut opts: HashMap<String, serde_json::Value> = HashMap::new();
        opts.insert(
            "string_k".to_string(),
            serde_json::Value::String("v".to_string()),
        );
        opts.insert("num_k".to_string(), serde_json::Value::Number(42.into()));
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{string_k} {num_k}".to_string()],
            options: opts,
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        // {string_k} is substituted; {num_k} is not a string option and remains
        assert!(prepared.prompt.contains("v {num_k}"));
    }

    #[test]
    fn test_prepare_multi_bind_verification_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{a} {b} {c}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([
                ("a".to_string(), "file://session/a.md".to_string()),
                ("b".to_string(), "file://session/b.md".to_string()),
                ("c".to_string(), "file://session/c.md".to_string()),
            ]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris.len(), 3);
        assert_eq!(prepared.expected_artifact_paths.len(), 3);
    }

    #[test]
    fn test_prepare_no_interpolation_map_runs_prompt_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Static prompt".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &reg, "", &fw).unwrap();
        assert!(prepared.prompt.ends_with("Static prompt"));
    }

    // ---- Stage trait forwarding ----

    #[test]
    fn test_agent_implements_stage_trait() {
        let agent = Agent {
            name: "test-agent".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::from([(
                "key".to_string(),
                serde_json::Value::String("val".to_string()),
            )]),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("bind-key".to_string(), "bind-val".to_string())]),
        };

        let stage: &dyn crate::stages::base::Stage = &agent;
        assert_eq!(stage.name(), "test-agent");
        assert_eq!(stage.stage_type(), "agent");
        assert_eq!(stage.path(), "");
        assert!(stage.client().is_none());
        assert!(!stage.client_explicit());
        assert!(stage.body().is_empty());
        assert_eq!(stage.bind_map().get("bind-key").unwrap(), "bind-val");
        assert_eq!(
            stage.options().get("key").unwrap(),
            &serde_json::Value::String("val".to_string())
        );
        assert_eq!(stage.skip_if_exists(), "");

        let extra = HashMap::new();
        let fw = HashMap::new();
        let result = stage.substitute_vars("{key}", &extra, &fw);
        assert_eq!(result, "val");
    }

    #[test]
    fn test_build_workspace_preamble_both_present() {
        let p = build_workspace_preamble("/work/dir", Some("/work/dir"));
        assert!(p.contains("Your working directory is: /work/dir"));
        assert!(!p.contains("Project worktree:"));

        let p2 = build_workspace_preamble("/tmp/run", Some("/repo"));
        assert!(p2.contains("Your working directory is: /tmp/run"));
        assert!(p2.contains("Project worktree: /repo"));
    }

    fn agent_with_bind(key: &str, uri: &str) -> Agent {
        Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([(key.to_string(), uri.to_string())]),
        }
    }

    #[test]
    fn test_prepare_agent_allows_recovering_from_stale_binding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));

        // Registered but its file is gone: a skip_if_exists producer must be
        // able to run (and commit) again.
        let uri = Uri::parse("artifact://plan.md").unwrap();
        let stale = reg.write_into_registry(&uri, "# plan").unwrap();
        std::fs::remove_file(&stale).unwrap();

        let agent = agent_with_bind("plan", "artifact://plan.md");
        let prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["plan"], "# new plan").unwrap();
        commit_agent(&prepared, &reg).unwrap();
        assert_eq!(
            reg.content("artifact://plan.md", None).unwrap(),
            "# new plan",
        );
    }

    #[test]
    fn test_commit_agent_rejects_missing_non_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        let err = commit_agent(&prepared, &reg).unwrap_err();
        assert!(matches!(err, AgentError::MissingArtifact { .. }));
        assert!(!reg.is_registered("artifact://out.md"));
    }

    #[test]
    fn test_commit_agent_rejects_empty_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["out"], "").unwrap();
        assert!(commit_agent(&prepared, &reg).is_err());
        assert!(!reg.is_registered("artifact://out.md"));
    }

    #[test]
    fn test_commit_agent_allows_missing_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out?", "artifact://out.md");
        let prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        commit_agent(&prepared, &reg).unwrap();
        assert!(!reg.is_registered("artifact://out.md"));
    }

    #[test]
    fn test_commit_agent_multi_output_missing_non_optional_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([
                ("a".to_string(), "artifact://a.md".to_string()),
                ("b".to_string(), "artifact://b.md".to_string()),
            ]),
        };
        let mut prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        // Pin iteration order so the missing bind is evaluated last.
        prepared.bind_uris.sort_by(|x, y| x.0.cmp(&y.0));
        std::fs::write(&prepared.bind_paths["a"], "content").unwrap();
        let err = commit_agent(&prepared, &reg).unwrap_err();
        assert!(matches!(err, AgentError::MissingArtifact { key, .. } if key == "b"));
        // The file that was written is still committed.
        assert!(reg.is_registered("artifact://a.md"));
        assert!(!reg.is_registered("artifact://b.md"));
    }

    #[test]
    fn test_commit_agent_multi_output_missing_optional_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([
                ("a".to_string(), "artifact://a.md".to_string()),
                ("b?".to_string(), "artifact://b.md".to_string()),
            ]),
        };
        let prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["a"], "content").unwrap();
        commit_agent(&prepared, &reg).unwrap();
        assert!(reg.is_registered("artifact://a.md"));
        assert!(!reg.is_registered("artifact://b.md"));
    }

    #[test]
    fn test_commit_agent_registers_produced_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["out"], "content").unwrap();
        commit_agent(&prepared, &reg).unwrap();
        assert!(reg.is_registered("artifact://out.md"));
    }

    // ---- from_dict tests ----

    fn agent_dict(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn from_dict_parses_prompt_and_maps() {
        let d = agent_dict(&[
            ("name", serde_json::json!("plan")),
            ("prompt", serde_json::json!(["first", "second"])),
            ("interpolation", serde_json::json!({"a": "artifact://a"})),
            ("bind", serde_json::json!({"b": "artifact://b"})),
            ("options", serde_json::json!({"model": "openai:gpt-5"})),
        ]);
        let agent = Agent::from_dict(&d).unwrap();
        assert_eq!(agent.name, "plan");
        assert_eq!(
            agent.prompts,
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(agent.interpolation_map.get("a").unwrap(), "artifact://a");
        assert_eq!(agent.bind_map.get("b").unwrap(), "artifact://b");
        assert_eq!(
            agent.options.get("model").unwrap(),
            &serde_json::json!("openai:gpt-5")
        );
    }

    #[test]
    fn from_dict_defaults_are_empty() {
        let agent = Agent::from_dict(&agent_dict(&[])).unwrap();
        assert_eq!(agent.name, "");
        assert!(agent.prompts.is_empty());
        assert!(agent.options.is_empty());
        assert!(agent.interpolation_map.is_empty());
        assert!(agent.bind_map.is_empty());
    }

    #[test]
    fn from_dict_rejects_in_and_out() {
        for key in ["in", "out"] {
            let d = agent_dict(&[
                ("name", serde_json::json!("s")),
                (key, serde_json::json!({})),
            ]);
            let err = Agent::from_dict(&d).unwrap_err();
            assert!(
                err.contains("'in'/'out' keys are no longer supported"),
                "{err}"
            );
        }
    }

    #[test]
    fn from_dict_rejects_non_mapping_interpolation() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("interpolation", serde_json::json!(["not", "a", "mapping"])),
        ]);
        let err = Agent::from_dict(&d).unwrap_err();
        assert!(err.contains("'interpolation' must be a mapping"), "{err}");
    }

    #[test]
    fn from_dict_rejects_null_interpolation() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("interpolation", serde_json::Value::Null),
        ]);
        assert!(Agent::from_dict(&d).is_err());
    }

    #[test]
    fn from_dict_rejects_non_string_bind_value() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("bind", serde_json::json!({"a": 42})),
        ]);
        let err = Agent::from_dict(&d).unwrap_err();
        assert!(err.contains("'bind' must be a mapping"), "{err}");
    }

    #[test]
    fn from_dict_rejects_framework_option_key_but_allows_model() {
        let bad = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::json!({"cwd": "/tmp"})),
        ]);
        let err = Agent::from_dict(&bad).unwrap_err();
        assert!(
            err.contains("collides with framework substitution variable"),
            "{err}"
        );

        let ok = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("options", serde_json::json!({"model": "openai:gpt-5"})),
        ]);
        assert!(Agent::from_dict(&ok).is_ok());
    }

    #[test]
    fn from_dict_rejects_non_string_prompt_item() {
        let d = agent_dict(&[
            ("name", serde_json::json!("s")),
            ("prompt", serde_json::json!(["ok", 7])),
        ]);
        let err = Agent::from_dict(&d).unwrap_err();
        assert!(err.contains("'prompt' must be a list of strings"), "{err}");
    }

    #[test]
    fn from_dict_rejects_missing_type_shape() {
        let d = agent_dict(&[("prompt", serde_json::json!("not a list"))]);
        assert!(Agent::from_dict(&d).is_err());
    }
}
