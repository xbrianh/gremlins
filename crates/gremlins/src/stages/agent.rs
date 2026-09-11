use std::collections::HashMap;

use thiserror::Error;

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::clients::protocol::CompletedRun;
use crate::stages::base;

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
    artifacts: &mut ArtifactRegistry,
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
        let path = artifacts
            .path_for_uri(&uri)
            .map_err(|e| AgentError::Generic {
                name: name.clone(),
                detail: e.to_string(),
            })?;
        bind_paths.insert(key.clone(), path);
        bind_uris.push((key, uri_str, optional));
    }

    // Merge: bind output paths shadow interpolation keys on collision
    let subst_vars: HashMap<String, String> = interpolation_map
        .iter()
        .chain(bind_paths.iter())
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
    artifacts: &mut ArtifactRegistry,
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
        "Relevant environment variables: $GREMLINS_WORKTREE_PATH, $GREMLIN_WORKSPACE_DIR, $GREMLINS_ARTIFACT_DIR"
            .to_string(),
    );
    parts.join("\n")
}

impl AgentPrepared {
    /// Assemble the final prompt: system prompt + workspace preamble + stage prompt.
    pub fn final_prompt(&self) -> String {
        let preamble = build_workspace_preamble(&self.cwd, self.worktree.as_deref());
        let scratch = crate::config::scratch_dir(None)
            .unwrap_or_else(|| crate::config::scratch_root(None));
        let sys = crate::config::agent_system_prompt(
            &crate::config::work_root(),
            &scratch,
            &crate::config::project_root(),
        );
        format!("{sys}\n\n{preamble}\n\n{}", self.prompt)
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

    fn register_file(reg: &mut ArtifactRegistry, name: &str, content: &str) -> String {
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
        let mut reg = make_registry(ad);
        register_file(&mut reg, "world", "world-value");
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert!(prepared.prompt.contains("world-value"));
    }

    #[test]
    fn test_prepare_framework_subs_win() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        register_file(&mut reg, "interp-src", "from-interp");
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert!(prepared.prompt.contains("from-fw"));
        assert!(!prepared.prompt.contains("from-interp"));
    }

    #[test]
    fn test_prepare_bind_shadows_interpolation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad.clone());
        register_file(&mut reg, "interp-val", "interp-val");
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        // bind_paths should contain the registered path, not "interp-val"
        assert!(prepared.bind_paths.contains_key("key"));
        let path = &prepared.bind_paths["key"];
        assert!(path.contains("out.md"));
        // The prompt should use the bind path (shadow)
        assert!(prepared.prompt.contains("out.md"));
        assert!(!prepared.prompt.contains("interp-val"));
    }

    #[test]
    fn test_prepare_missing_interpolation_key_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{missing}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([("missing".to_string(), "nonexistent".to_string())]),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let err = prepare_agent(&agent, &mut reg, "", &fw).unwrap_err();
        assert!(matches!(err, AgentError::Resolve { .. }));
    }

    #[test]
    fn test_prepare_loop_iter_in_bind_uri() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
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
        let prepared = prepare_agent(&agent, &mut reg, "my-agent~3", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].1, "artifact://my-agent~3/out.txt");
    }

    #[test]
    fn test_prepare_loop_iter_in_interpolation_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad.clone());
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
        let prepared = prepare_agent(&agent, &mut reg, "my-agent~2", &fw).unwrap();
        assert!(prepared.prompt.contains("Plan: # Plan"));
    }

    #[test]
    fn test_prepare_optional_bind_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{result}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::from([("result?".to_string(), "file://session/out.md".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].0, "result");
        assert!(prepared.bind_uris[0].2); // optional
    }

    #[test]
    fn test_prepare_model_substituted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        register_file(&mut reg, "openai", "openai");
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert_eq!(prepared.model, Some("openai:{variant}".to_string()));
    }

    #[test]
    fn test_prepare_model_none_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert!(prepared.model.is_none());
    }

    #[test]
    fn test_prepare_workspace_preamble_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        prepared.cwd = String::new();
        prepared.worktree = None;
        let preamble = build_workspace_preamble(&prepared.cwd, prepared.worktree.as_deref());
        assert_eq!(preamble, "Relevant environment variables: $GREMLINS_WORKTREE_PATH, $GREMLIN_WORKSPACE_DIR, $GREMLINS_ARTIFACT_DIR");
        let full = format!("{preamble}\n\n{}", prepared.prompt);
        assert!(!full.contains("Your working directory is"));
    }

    #[test]
    fn test_prepare_workspace_preamble_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
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
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
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
        let mut reg = make_registry(ad);
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris[0].0, "my-agent");
        assert!(prepared.bind_uris[0].1.contains("my-agent.md"));
    }

    #[test]
    fn test_prepare_prompts_joined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert_eq!(prepared.prompt, "Line 1\n\nLine 2\n\nLine 3");
    }

    #[test]
    fn test_prepare_hyphen_normalization_in_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        register_file(&mut reg, "value", "value");
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert!(prepared.prompt.contains("value"));
    }

    #[test]
    fn test_prepare_options_string_filtering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        // {string_k} is substituted; {num_k} is not a string option and remains
        assert!(prepared.prompt.contains("v {num_k}"));
    }

    #[test]
    fn test_prepare_multi_bind_verification_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
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
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
        assert_eq!(prepared.bind_uris.len(), 3);
        assert_eq!(prepared.expected_artifact_paths.len(), 3);
    }

    #[test]
    fn test_prepare_no_interpolation_map_runs_prompt_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let mut reg = make_registry(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Static prompt".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            bind_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &mut reg, "", &fw).unwrap();
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
        let mut reg = make_registry(ensure_artifact_dir(&tmp));

        // Registered but its file is gone: a skip_if_exists producer must be
        // able to run (and commit) again.
        let uri = Uri::parse("artifact://plan.md").unwrap();
        let stale = reg.write_into_registry(&uri, "# plan").unwrap();
        std::fs::remove_file(&stale).unwrap();

        let agent = agent_with_bind("plan", "artifact://plan.md");
        let prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["plan"], "# new plan").unwrap();
        commit_agent(&prepared, &mut reg).unwrap();
        assert_eq!(
            reg.content("artifact://plan.md", None).unwrap(),
            "# new plan",
        );
    }

    #[test]
    fn test_commit_agent_rejects_missing_non_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        let err = commit_agent(&prepared, &mut reg).unwrap_err();
        assert!(matches!(err, AgentError::MissingArtifact { .. }));
        assert!(!reg.is_registered("artifact://out.md"));
    }

    #[test]
    fn test_commit_agent_rejects_empty_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["out"], "").unwrap();
        assert!(commit_agent(&prepared, &mut reg).is_err());
        assert!(!reg.is_registered("artifact://out.md"));
    }

    #[test]
    fn test_commit_agent_allows_missing_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out?", "artifact://out.md");
        let prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        commit_agent(&prepared, &mut reg).unwrap();
        assert!(!reg.is_registered("artifact://out.md"));
    }

    #[test]
    fn test_commit_agent_multi_output_missing_non_optional_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = make_registry(ensure_artifact_dir(&tmp));
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
        let mut prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        // Pin iteration order so the missing bind is evaluated last.
        prepared.bind_uris.sort_by(|x, y| x.0.cmp(&y.0));
        std::fs::write(&prepared.bind_paths["a"], "content").unwrap();
        let err = commit_agent(&prepared, &mut reg).unwrap_err();
        assert!(matches!(err, AgentError::MissingArtifact { key, .. } if key == "b"));
        // The file that was written is still committed.
        assert!(reg.is_registered("artifact://a.md"));
        assert!(!reg.is_registered("artifact://b.md"));
    }

    #[test]
    fn test_commit_agent_multi_output_missing_optional_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = make_registry(ensure_artifact_dir(&tmp));
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
        let prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["a"], "content").unwrap();
        commit_agent(&prepared, &mut reg).unwrap();
        assert!(reg.is_registered("artifact://a.md"));
        assert!(!reg.is_registered("artifact://b.md"));
    }

    #[test]
    fn test_commit_agent_registers_produced_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = make_registry(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &mut reg, "", &HashMap::new()).unwrap();
        std::fs::write(&prepared.bind_paths["out"], "content").unwrap();
        commit_agent(&prepared, &mut reg).unwrap();
        assert!(reg.is_registered("artifact://out.md"));
    }
}
