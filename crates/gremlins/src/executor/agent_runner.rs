use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::artifacts::resolve::{resolve_interpolation_map, ResolveError};
use crate::artifacts::uri::Uri;
use crate::definition::Agent;
use crate::executor::state::StateStore;
use crate::executor::vars;

/// Hidden, programmatic-only option key that disables tool-path sandboxing for
/// a stage's agent loop. Not part of `FRAMEWORK_KEYS`, not documented in any
/// schema, and not surfaced in tool descriptions. It is read only here and set
/// only by the chat gremlin builder.
pub(crate) const SANDBOX_ALLOW_ALL_KEY: &str = "sandbox_allow_all";

// ---------------------------------------------------------------------------
// AgentPrepared — fully resolved pre-run state
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct AgentPrepared {
    pub name: String,
    pub prompt: String,
    pub model: Option<String>,
    pub(crate) output_paths: HashMap<String, String>,
    /// (key, uri_str, optional)
    pub(crate) output_uris: Vec<(String, String, bool)>,
    pub expected_artifact_paths: Vec<String>,
    pub cwd: String,
    pub artifact_dir: String,
    /// Whether the stage opted out of tool-path sandboxing via the hidden
    /// [`SANDBOX_ALLOW_ALL_KEY`] option.
    pub allow_all: bool,
}

// ---------------------------------------------------------------------------
// AgentError
// ---------------------------------------------------------------------------

#[derive(Error, Debug)]
pub enum AgentError {
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

pub async fn prepare_agent(
    agent: &Agent,
    main_state: &dyn StateStore,
    local_state: &dyn StateStore,
    loop_iter: &str,
    framework_subs: &HashMap<String, String>,
) -> Result<AgentPrepared, AgentError> {
    let name = &agent.name;
    let str_opts = vars::string_options(&agent.options);

    // Split interpolation: content() entries resolved against main_state,
    // bare-URI (filepath) entries resolved against local_state.
    let (content_map, filepath_map) =
        crate::artifacts::resolve::split_interpolation_map(&agent.interpolation_map);

    let content_interpolated = resolve_interpolation_map(main_state, &content_map, loop_iter)
        .await
        .map_err(|e| AgentError::Resolve {
            name: name.clone(),
            source: e,
        })?;

    let filepath_interpolated = resolve_interpolation_map(local_state, &filepath_map, loop_iter)
        .await
        .map_err(|e| AgentError::Resolve {
            name: name.clone(),
            source: e,
        })?;

    // Merge: filepath shadows content on key collision.
    let mut interpolation_map: HashMap<String, String> = content_interpolated;
    interpolation_map.extend(filepath_interpolated);

    let mut output_paths: HashMap<String, String> = HashMap::new();
    let mut output_uris: Vec<(String, String, bool)> = Vec::new();
    for (raw_key, raw_uri_str) in &agent.outputs_map {
        let k = vars::substitute_vars(raw_key, &str_opts, &interpolation_map, framework_subs);
        let optional = k.ends_with('?');
        let key = k.trim_end_matches('?').to_string();
        let mut uri_str =
            vars::substitute_vars(raw_uri_str, &str_opts, &interpolation_map, framework_subs);
        if !loop_iter.is_empty() {
            uri_str = uri_str.replace("{loop_iter}", loop_iter);
        }
        let uri = Uri::parse(&uri_str).map_err(|e| AgentError::Generic {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        // Optional binds are skipped when a sibling already committed the URI.
        // Check against the main state (the authority for what exists).
        if !optional && main_state.is_registered(&uri_str).await {
            return Err(AgentError::Generic {
                name: name.clone(),
                detail: format!("artifact {uri_str:?} is already produced — duplicate producer"),
            });
        }
        let path = local_state
            .path_for_uri(&uri)
            .await
            .map_err(|e| AgentError::Generic {
                name: name.clone(),
                detail: e.to_string(),
            })?;
        output_paths.insert(key.clone(), path);
        output_uris.push((key, uri_str, optional));
    }

    // Merge: bind output paths shadow interpolation keys on collision
    let subst_vars: HashMap<String, String> = interpolation_map
        .iter()
        .chain(output_paths.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let template = agent.prompts.join("\n\n").trim_end().to_string();
    let prompt = vars::substitute_vars(&template, &str_opts, &subst_vars, framework_subs);

    // Model substitution
    let model = agent
        .options
        .get("model")
        .and_then(|v| v.as_str())
        .map(|raw| vars::substitute_vars(raw, &str_opts, &subst_vars, framework_subs));

    let expected_artifact_paths: Vec<String> = output_paths.values().cloned().collect();

    // Read the hidden sandbox opt-out. It is honored only when the
    // gremlin's kind is "chat" (set programmatically by the chat builder,
    // never by user YAML), so containment cannot be disabled by arbitrary
    // user-defined stages.
    let allow_all = framework_subs.get("kind").map(|s| s.as_str()) == Some("chat")
        && agent
            .options
            .get(SANDBOX_ALLOW_ALL_KEY)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

    Ok(AgentPrepared {
        name: name.clone(),
        prompt,
        model,
        output_paths,
        output_uris,
        expected_artifact_paths,
        cwd: String::new(),
        artifact_dir: String::new(),
        allow_all,
    })
}

// ---------------------------------------------------------------------------
// commit_agent
// ---------------------------------------------------------------------------

/// Commit produced artifacts into the localized state store. Every non-optional
/// bind must have a file that exists; only extant files are committed.
/// Optional binds may be absent.
pub async fn commit_agent(
    prepared: &AgentPrepared,
    local_state: &dyn StateStore,
) -> Result<(), AgentError> {
    for (key, uri_str, optional) in &prepared.output_uris {
        let path = &prepared.output_paths[key];
        let produced = tokio::fs::metadata(path).await.is_ok();
        if produced {
            local_state
                .commit(uri_str, path)
                .await
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
// Workspace preamble assembly
// ---------------------------------------------------------------------------

pub(crate) fn build_workspace_preamble(cwd: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !cwd.is_empty() {
        parts.push(format!("Your working directory is: {cwd}"));
    }
    parts.push("Relevant environment variable: $GREMLIN_WORKDIR".to_string());
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Chat repository context
// ---------------------------------------------------------------------------

/// Run a git command with `cwd` and return its trimmed stdout, or `None` on
/// any failure (missing binary, not a repo, detached/broken state, permission
/// problem). Never panics, never surfaces raw git errors.
fn git_output(cwd: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Whether `cwd` is inside a git work tree. A missing `git` binary is treated
/// exactly like a non-git directory: `false`, so every git section is skipped.
fn is_git_repo(cwd: &str) -> bool {
    git_output(cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// A bounded, read-only snapshot of the repository context for a chat turn.
///
/// Git sections are gated behind a single `rev-parse --is-inside-work-tree`
/// check and are omitted (never an error) when the directory is not a git
/// repository, `git` is missing, or any individual command fails. The
/// top-level directory/file listing is produced independently of git and
/// includes every visible entry — non-code files are listed by name, never
/// read into the prompt.
pub(crate) fn build_chat_context(cwd: &str) -> String {
    let mut sections: Vec<String> = Vec::new();

    if is_git_repo(cwd) {
        if let Some(branch) = git_output(cwd, &["branch", "--show-current"]) {
            sections.push(format!("Current branch: {branch}"));
        }

        if let Some(remotes) = git_output(cwd, &["remote", "-v"]) {
            sections.push(format!("Remotes:\n{remotes}"));
        }

        if let Some(status) = git_output(cwd, &["status", "--short"]) {
            sections.push(format!("Git status (short):\n{status}"));
        }

        if let Some(log) = git_output(cwd, &["log", "--oneline", "-n", "5"]) {
            sections.push(format!("Recent commits:\n{log}"));
        }

        // Diff stat against upstream/base when available.
        if let Some(stat) = git_output(cwd, &["diff", "--stat", "@{upstream}"]) {
            if !stat.trim().is_empty() {
                sections.push(format!("Diff stat vs upstream:\n{stat}"));
            }
        }
    }

    if let Some(listing) = top_level_listing(cwd) {
        sections.push(format!("Top-level directory listing:\n{listing}"));
    }

    sections.join("\n\n")
}

/// A bounded listing of the top-level entries in `cwd`, each tagged by file
/// type (directory / file / symlink). Every visible entry is included; no
/// source-code or known-language filtering is applied.
fn top_level_listing(cwd: &str) -> Option<String> {
    let entries = std::fs::read_dir(cwd).ok()?;
    let mut rows: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type().ok();
        let tag = if ft.as_ref().is_some_and(|t| t.is_dir()) {
            "dir"
        } else if ft.as_ref().is_some_and(|t| t.is_symlink()) {
            "symlink"
        } else {
            "file"
        };
        rows.push(format!("{tag}  {name}"));
    }
    if rows.is_empty() {
        return None;
    }
    rows.sort();
    Some(rows.join("\n"))
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
        let preamble = build_workspace_preamble(&self.cwd);
        format!("{preamble}\n\n{}", self.prompt)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::executor::state::FileSystemStateStore;

    // ---- prepare_agent tests ----

    fn make_state_store(artifact_dir: PathBuf) -> FileSystemStateStore {
        let state_file = artifact_dir.parent().unwrap().join("state.json");
        std::fs::write(&state_file, "{}").unwrap();
        FileSystemStateStore::from_path(state_file.parent().unwrap().to_path_buf())
    }

    async fn register_file(store: &FileSystemStateStore, name: &str, content: &str) -> String {
        let uri = Uri::parse(&format!("artifact://{name}")).unwrap();
        store.write_into_registry(&uri, content).await.unwrap()
    }

    fn ensure_artifact_dir(tmp: &tempfile::TempDir) -> PathBuf {
        let ad = tmp.path().join("artifacts");
        std::fs::create_dir_all(&ad).unwrap();
        ad
    }

    #[tokio::test]
    async fn test_prepare_basic_prompt_substitution() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        register_file(&store, "world", "world-value").await;
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Hello {var}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "var".to_string(),
                r#"content("artifact://world")"#.to_string(),
            )]),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert!(prepared.prompt.contains("world-value"));
    }

    #[tokio::test]
    async fn test_prepare_framework_subs_win() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        register_file(&store, "interp-src", "from-interp").await;
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Hello {name}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "name".to_string(),
                r#"content("artifact://interp-src")"#.to_string(),
            )]),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::from([("name".to_string(), "from-fw".to_string())]);
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert!(prepared.prompt.contains("from-fw"));
        assert!(!prepared.prompt.contains("from-interp"));
    }

    #[tokio::test]
    async fn test_prepare_bind_shadows_interpolation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad.clone());
        register_file(&store, "interp-val", "interp-val").await;
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{key}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "key".to_string(),
                r#"content("artifact://interp-val")"#.to_string(),
            )]),
            outputs_map: HashMap::from([("key".to_string(), "artifact://out.md".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        // output_paths should contain the registered path, not "interp-val"
        assert!(prepared.output_paths.contains_key("key"));
        let path = &prepared.output_paths["key"];
        assert!(path.contains("out.md"));
        // The prompt should use the bind path (shadow)
        assert!(prepared.prompt.contains("out.md"));
        assert!(!prepared.prompt.contains("interp-val"));
    }

    #[tokio::test]
    async fn test_prepare_missing_interpolation_key_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{missing}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([("missing".to_string(), "nonexistent".to_string())]),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let err = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Resolve { .. }));
    }

    #[tokio::test]
    async fn test_prepare_loop_iter_in_bind_uri() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{out}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([(
                "out".to_string(),
                "artifact://{loop_iter}/out.txt".to_string(),
            )]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "my-agent~3", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.output_uris[0].1, "artifact://my-agent~3/out.txt");
    }

    #[tokio::test]
    async fn test_prepare_loop_iter_in_interpolation_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad.clone());
        // Pre-register the artifact that content() will look up
        let plan_uri = Uri::parse("artifact://my-agent~2/plan.md").unwrap();
        store
            .write_into_registry(&plan_uri, "# Plan")
            .await
            .unwrap();
        // Register bind for the output so verify doesn't fail
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Plan: {plan}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "plan".to_string(),
                r#"content("artifact://{loop_iter}/plan.md")"#.to_string(),
            )]),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "my-agent~2", &fw)
            .await
            .unwrap();
        assert!(prepared.prompt.contains("Plan: # Plan"));
    }

    #[tokio::test]
    async fn test_prepare_optional_bind_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{result}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([("result?".to_string(), "artifact://out.md".to_string())]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.output_uris[0].0, "result");
        assert!(prepared.output_uris[0].2); // optional
    }

    #[tokio::test]
    async fn test_prepare_model_substituted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        register_file(&store, "openai", "openai").await;
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
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.model, Some("openai:{variant}".to_string()));
    }

    #[tokio::test]
    async fn test_prepare_model_none_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert!(prepared.model.is_none());
    }

    #[tokio::test]
    async fn test_prepare_workspace_preamble_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        prepared.cwd = String::new();
        let preamble = build_workspace_preamble(&prepared.cwd);
        assert_eq!(preamble, "Relevant environment variable: $GREMLIN_WORKDIR");
        let full = format!("{preamble}\n\n{}", prepared.prompt);
        assert!(!full.contains("Your working directory is"));
    }

    #[tokio::test]
    async fn test_prepare_workspace_preamble_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let mut prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        prepared.cwd = "/work".to_string();
        let preamble = build_workspace_preamble(&prepared.cwd);
        let full = format!("{preamble}\n\n{}", prepared.prompt);
        assert!(full.contains("Your working directory is: /work"));
        assert!(!full.contains("Project worktree:"));
    }

    #[tokio::test]
    async fn test_prepare_name_substitution_in_bind_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{my-agent}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([(
                "{name}".to_string(),
                "artifact://{name}.md".to_string(),
            )]),
        };
        let fw = HashMap::from([("name".to_string(), "my-agent".to_string())]);
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.output_uris[0].0, "my-agent");
        assert!(prepared.output_uris[0].1.contains("my-agent.md"));
    }

    #[tokio::test]
    async fn test_prepare_prompts_joined() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec![
                "Line 1".to_string(),
                "Line 2".to_string(),
                "Line 3".to_string(),
            ],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.prompt, "Line 1\n\nLine 2\n\nLine 3");
    }

    #[tokio::test]
    async fn test_prepare_hyphen_normalization_in_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        register_file(&store, "value", "value").await;
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{child-plan}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::from([(
                "child_plan".to_string(),
                "artifact://value".to_string(),
            )]),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert!(prepared.prompt.contains("value"));
    }

    #[tokio::test]
    async fn test_prepare_options_string_filtering() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
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
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        // {string_k} and {num_k} are both substituted
        assert!(prepared.prompt.contains("v 42"));
    }

    #[tokio::test]
    async fn test_prepare_multi_bind_verification_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["{a} {b} {c}".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([
                ("a".to_string(), "artifact://a.md".to_string()),
                ("b".to_string(), "artifact://b.md".to_string()),
                ("c".to_string(), "artifact://c.md".to_string()),
            ]),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert_eq!(prepared.output_uris.len(), 3);
        assert_eq!(prepared.expected_artifact_paths.len(), 3);
    }

    #[tokio::test]
    async fn test_prepare_no_interpolation_map_runs_prompt_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ad = ensure_artifact_dir(&tmp);
        let store = make_state_store(ad);
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["Static prompt".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::new(),
        };
        let fw = HashMap::new();
        let prepared = prepare_agent(&agent, &store, &store, "", &fw)
            .await
            .unwrap();
        assert!(prepared.prompt.ends_with("Static prompt"));
    }

    #[test]
    fn test_build_workspace_preamble_both_present() {
        let p = build_workspace_preamble("/work/dir");
        assert!(p.contains("Your working directory is: /work/dir"));
        assert!(!p.contains("Project worktree:"));

        let p2 = build_workspace_preamble("/tmp/run");
        assert!(p2.contains("Your working directory is: /tmp/run"));
        assert!(!p2.contains("Project worktree:"));
    }

    fn agent_with_bind(key: &str, uri: &str) -> Agent {
        Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([(key.to_string(), uri.to_string())]),
        }
    }

    #[tokio::test]
    async fn test_commit_agent_rejects_missing_non_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = make_state_store(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &store, &store, "", &HashMap::new())
            .await
            .unwrap();
        let err = commit_agent(&prepared, &store).await.unwrap_err();
        assert!(matches!(err, AgentError::MissingArtifact { .. }));
        assert!(!store.is_registered("artifact://out.md").await);
    }

    #[tokio::test]
    async fn test_commit_agent_accepts_empty_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = make_state_store(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &store, &store, "", &HashMap::new())
            .await
            .unwrap();
        std::fs::write(&prepared.output_paths["out"], "").unwrap();
        commit_agent(&prepared, &store).await.unwrap();
        assert!(store.is_registered("artifact://out.md").await);
    }

    #[tokio::test]
    async fn test_commit_agent_allows_missing_optional() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = make_state_store(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out?", "artifact://out.md");
        let prepared = prepare_agent(&agent, &store, &store, "", &HashMap::new())
            .await
            .unwrap();
        commit_agent(&prepared, &store).await.unwrap();
        assert!(!store.is_registered("artifact://out.md").await);
    }

    #[tokio::test]
    async fn test_commit_agent_multi_output_missing_non_optional_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = make_state_store(ensure_artifact_dir(&tmp));
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([
                ("a".to_string(), "artifact://a.md".to_string()),
                ("b".to_string(), "artifact://b.md".to_string()),
            ]),
        };
        let mut prepared = prepare_agent(&agent, &store, &store, "", &HashMap::new())
            .await
            .unwrap();
        // Pin iteration order so the missing bind is evaluated last.
        prepared.output_uris.sort_by(|x, y| x.0.cmp(&y.0));
        std::fs::write(&prepared.output_paths["a"], "content").unwrap();
        let err = commit_agent(&prepared, &store).await.unwrap_err();
        assert!(matches!(err, AgentError::MissingArtifact { key, .. } if key == "b"));
        // The file that was written is still committed.
        assert!(store.is_registered("artifact://a.md").await);
        assert!(!store.is_registered("artifact://b.md").await);
    }

    #[tokio::test]
    async fn test_commit_agent_multi_output_missing_optional_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = make_state_store(ensure_artifact_dir(&tmp));
        let agent = Agent {
            name: "test".to_string(),
            prompts: vec!["hi".to_string()],
            options: HashMap::new(),
            interpolation_map: HashMap::new(),
            outputs_map: HashMap::from([
                ("a".to_string(), "artifact://a.md".to_string()),
                ("b?".to_string(), "artifact://b.md".to_string()),
            ]),
        };
        let prepared = prepare_agent(&agent, &store, &store, "", &HashMap::new())
            .await
            .unwrap();
        std::fs::write(&prepared.output_paths["a"], "content").unwrap();
        commit_agent(&prepared, &store).await.unwrap();
        assert!(store.is_registered("artifact://a.md").await);
        assert!(!store.is_registered("artifact://b.md").await);
    }

    #[tokio::test]
    async fn test_commit_agent_registers_produced_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = make_state_store(ensure_artifact_dir(&tmp));
        let agent = agent_with_bind("out", "artifact://out.md");
        let prepared = prepare_agent(&agent, &store, &store, "", &HashMap::new())
            .await
            .unwrap();
        std::fs::write(&prepared.output_paths["out"], "content").unwrap();
        commit_agent(&prepared, &store).await.unwrap();
        assert!(store.is_registered("artifact://out.md").await);
    }

    // ---- build_chat_context tests ----

    /// Run `git` in `dir`, ignoring failures (e.g. git unavailable).
    fn git(dir: &std::path::Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn chat_context_in_git_repo_includes_git_sections_and_listing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !git(&repo, &["init", "-q"]) {
            eprintln!("git unavailable; skipping chat_context_in_git_repo");
            return;
        }
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "# hi\n").unwrap();
        std::fs::write(repo.join("notes.txt"), "notes\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "init"]);

        let cwd = repo.to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(ctx.contains("Current branch:"), "ctx: {ctx}");
        assert!(ctx.contains("Recent commits:"), "ctx: {ctx}");
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        // Non-code files are listed by name, not filtered out.
        assert!(ctx.contains("notes.txt"), "ctx: {ctx}");
        assert!(ctx.contains("README.md"), "ctx: {ctx}");
    }

    #[test]
    fn chat_context_outside_git_repo_omits_git_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "a").unwrap();
        std::fs::write(dir.join("b.csv"), "b").unwrap();
        std::fs::write(dir.join("image.png"), b"\x89PNG").unwrap();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();

        let cwd = dir.to_string_lossy().into_owned();
        let ctx = build_chat_context(&cwd);

        assert!(!ctx.contains("Current branch:"), "ctx: {ctx}");
        assert!(!ctx.contains("Recent commits:"), "ctx: {ctx}");
        assert!(!ctx.contains("Git status"), "ctx: {ctx}");
        assert!(!ctx.contains("Remotes:"), "ctx: {ctx}");
        // The listing is still produced, with non-code files included.
        assert!(ctx.contains("Top-level directory listing:"), "ctx: {ctx}");
        assert!(ctx.contains("a.md"), "ctx: {ctx}");
        assert!(ctx.contains("b.csv"), "ctx: {ctx}");
        assert!(ctx.contains("image.png"), "ctx: {ctx}");
        assert!(ctx.contains("subdir"), "ctx: {ctx}");
    }

    #[test]
    fn chat_context_nonexistent_dir_is_empty() {
        let ctx = build_chat_context("/nonexistent/gremlins/chat/root");
        assert!(ctx.is_empty(), "ctx should be empty, got: {ctx}");
    }
}
