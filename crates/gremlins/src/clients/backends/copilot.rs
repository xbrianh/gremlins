use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::providers::copilot::{self, CopilotIntent};

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::openai_protocol;
use crate::clients::protocol::CompletedRun;
use crate::clients::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use crate::clients::task::TaskModelSelector;

const PROVIDER_NAME: &str = "copilot";
const DEFAULT_MODEL: &str = "gpt-4o";

// ── Auth source ──────────────────────────────────────────────────────────

/// Which credential source was used to build the Copilot client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopilotAuthSource {
    /// `GITHUB_COPILOT_API_KEY` env var.
    GitHubCopilotApiKey,
    /// `COPILOT_API_KEY` env var.
    CopilotApiKey,
    /// `COPILOT_GITHUB_ACCESS_TOKEN` env var.
    CopilotGitHubAccessToken,
    /// `GITHUB_TOKEN` env var.
    GitHubToken,
    /// `providers.json` `"copilot"` entry (`api-key` field).
    ProvidersJson,
    /// `providers.json` `"copilot"` entry (`pat` field).
    ProvidersJsonPat,
    /// Auto-discovered from `~/.config/github-copilot/apps.json`.
    AppsJson,
}

/// Resolve credentials and build a Copilot client, returning the client and
/// which source won.  OAuth device-code flow is disabled — gremlins run
/// unattended.
///
/// Auth precedence: `GITHUB_COPILOT_API_KEY` → `COPILOT_API_KEY` →
/// `COPILOT_GITHUB_ACCESS_TOKEN` → `GITHUB_TOKEN` → `providers.json`
/// `"copilot"` entry (`api-key` then `pat`) →
/// `~/.config/github-copilot/apps.json` → error.
fn resolve_auth() -> Result<(copilot::Client, CopilotAuthSource), String> {
    let api_key = crate::config::copilot_api_key();
    let github_token = crate::config::copilot_github_token();

    if let Some(key) = api_key {
        let client = copilot::Client::builder()
            .api_key(key)
            .allow_device_flow(false)
            .build()
            .map_err(|e| format!("{e}"))?;
        // Determine which env var supplied the key.
        let source = if std::env::var("GITHUB_COPILOT_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
        {
            CopilotAuthSource::GitHubCopilotApiKey
        } else {
            CopilotAuthSource::CopilotApiKey
        };
        return Ok((client, source));
    }

    if let Some(token) = github_token {
        let client = copilot::Client::builder()
            .github_access_token(token)
            .allow_device_flow(false)
            .build()
            .map_err(|e| format!("{e}"))?;
        let source = if std::env::var("COPILOT_GITHUB_ACCESS_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_some()
        {
            CopilotAuthSource::CopilotGitHubAccessToken
        } else {
            CopilotAuthSource::GitHubToken
        };
        return Ok((client, source));
    }

    // providers.json: try api-key first, then pat.
    if let Some(key) = crate::config::api_key("", PROVIDER_NAME) {
        let client = copilot::Client::builder()
            .api_key(key)
            .allow_device_flow(false)
            .build()
            .map_err(|e| format!("{e}"))?;
        return Ok((client, CopilotAuthSource::ProvidersJson));
    }

    if let Some(token) = crate::config::pat(PROVIDER_NAME) {
        let client = copilot::Client::builder()
            .github_access_token(token)
            .allow_device_flow(false)
            .build()
            .map_err(|e| format!("{e}"))?;
        return Ok((client, CopilotAuthSource::ProvidersJsonPat));
    }

    // Auto-discover OAuth token from the Copilot extension's apps.json.
    if let Some(token) = crate::config::copilot_oauth_token() {
        let client = copilot::Client::builder()
            .github_access_token(token)
            .allow_device_flow(false)
            .build()
            .map_err(|e| format!("{e}"))?;
        return Ok((client, CopilotAuthSource::AppsJson));
    }

    Err(format!(
        "no credentials for provider '{PROVIDER_NAME}': set GITHUB_COPILOT_API_KEY, \
         COPILOT_API_KEY, COPILOT_GITHUB_ACCESS_TOKEN, GITHUB_TOKEN, or add an \
         entry with \"api-key\" or \"pat\" in {}",
        crate::config::user_config_root()
            .join("providers.json")
            .display(),
    ))
}

// ── CopilotRunState ──────────────────────────────────────────────────────

struct CopilotRunState {
    client: copilot::Client,
    model: String,
    tool_filter: Option<Vec<String>>,
    intent: Option<CopilotIntent>,
    strict_tools: bool,
    tool_result_array_content: bool,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    next_id: AtomicU64,
    log_label: String,
}

/// Parse Copilot-specific extra params from the client-spec parameter map.
fn parse_extra_params(
    extra_params: &indexmap::IndexMap<String, String>,
) -> (Option<CopilotIntent>, bool, bool) {
    let intent = extra_params.get("intent").map(|v| match v.as_str() {
        "edits" => CopilotIntent::Edits,
        _ => CopilotIntent::Panel,
    });
    let strict_tools = extra_params
        .get("strict_tools")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let tool_result_array_content = extra_params
        .get("tool_result_array_content")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    (intent, strict_tools, tool_result_array_content)
}

impl CopilotRunState {
    fn make_model(&self, override_model: Option<&str>) -> copilot::CompletionModel {
        let model_name = match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        };
        let mut mdl = self.client.completion_model(&model_name);
        if let Some(intent) = self.intent {
            mdl = mdl.with_intent(intent);
        }
        if self.strict_tools {
            mdl = mdl.with_strict_tools();
        }
        if self.tool_result_array_content {
            mdl = mdl.with_tool_result_array_content();
        }
        mdl
    }
}

// ── CopilotBackend ───────────────────────────────────────────────────────

pub struct CopilotBackend {
    state: CopilotRunState,
}

impl CopilotBackend {
    /// Build a Copilot backend.
    ///
    /// Auth precedence: `GITHUB_COPILOT_API_KEY` → `COPILOT_API_KEY` →
    /// `COPILOT_GITHUB_ACCESS_TOKEN` → `GITHUB_TOKEN` → `providers.json`
    /// `"copilot"` entry → error. OAuth is disabled — gremlins run
    /// unattended.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let (client, _auth_source) = resolve_auth()?;

        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.to_string()
        };

        let tool_filter = openai_protocol::tool_filter(native_block);

        let (intent, strict_tools, tool_result_array_content) =
            parse_extra_params(extra_params);

        Ok(Arc::new(Self {
            state: CopilotRunState {
                client,
                model,
                tool_filter,
                intent,
                strict_tools,
                tool_result_array_content,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "CopilotBackend".to_string(),
            },
        }))
    }
}

/// Build a `TaskModelSelector` for the Copilot backend, or `None` when
/// `config.json` declares no `task-clients` entries this backend can serve.
fn copilot_task_model_selector(
    client: &copilot::Client,
    task_clients_exact: &HashMap<String, String>,
    task_clients_prefix: &HashMap<String, String>,
) -> Option<TaskModelSelector<copilot::CompletionModel>> {
    if task_clients_exact.is_empty() && task_clients_prefix.is_empty() {
        return None;
    }

    let exact = task_clients_exact.clone();
    let prefix = task_clients_prefix.clone();
    let client = client.clone();
    TaskModelSelector::new(
        exact,
        prefix,
        Arc::new(move |spec: &str| {
            let (provider, model) = openai_protocol::provider_and_model(spec)?;
            if provider == PROVIDER_NAME {
                Some(client.completion_model(model))
            } else {
                log::warn!(
                    "task-clients entry spec {spec:?} names provider {provider:?}, but this \
                     backend serves {PROVIDER_NAME:?} — falling back to parent model"
                );
                None
            }
        }),
    )
}

#[async_trait]
impl Backend for CopilotBackend {
    async fn run(&self, mut params: RunParams) -> Result<CompletedRun, ClientError> {
        validate_max_retries(params.max_retries).map_err(|m| ClientError::Runtime { message: m })?;

        // Snatch interactive session before params.clone() drops the receiver.
        let interactive = params.interactive.take();

        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };
        let mut ctx = RunContext {
            params: params.clone(),
            prefix: prefix.clone(),
            idle_timeout,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
        };
        ctx.params.interactive = interactive;
        *self.state.last_ctx.lock().unwrap() = Some(ctx.clone());

        let prompt = Mutex::new(params.prompt.clone());
        let timeout_prompt = params.on_timeout_prompt.clone();
        let backoff = &STREAM_IDLE_BACKOFF[..params.max_retries];

        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);

        let task_selector = copilot_task_model_selector(
            &self.state.client,
            &ctx.params.task_clients_exact,
            &ctx.params.task_clients_prefix,
        );

        retry::with_retry(
            backoff,
            |e: &ClientError| {
                matches!(
                    e,
                    ClientError::Timeout { .. } | ClientError::ApiServerError { .. }
                )
            },
            |attempt, e, wait| {
                let next = match e {
                    ClientError::Timeout { .. } => timeout_prompt
                        .clone()
                        .unwrap_or_else(|| prompt.lock().unwrap().clone()),
                    _ => prompt.lock().unwrap().clone(),
                };
                *prompt.lock().unwrap() = next;
                let cause = match e {
                    ClientError::Timeout { .. } => "stream idle timeout",
                    ClientError::ApiServerError { .. } => "transient-error",
                    _ => "error",
                };
                log::warn!(
                    "{prefix}stream {cause}, retrying in {wait}s ({}/{})...",
                    attempt + 1,
                    params.max_retries
                );
            },
            || {
                let p = prompt.lock().unwrap().clone();
                let ctx = ctx.clone();
                let cancel = cancel.clone();
                let task_selector = task_selector.clone();
                async move {
                    if cancel.is_cancelled() {
                        return Err(ClientError::Runtime {
                            message: "cancelled".into(),
                        });
                    }

                    let gremlin_id = ctx.params.gremlin_id.clone().unwrap_or_default();
                    let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
                    self.state
                        .cancels
                        .lock()
                        .unwrap()
                        .entry(gremlin_id.clone())
                        .or_default()
                        .insert(id, cancel.clone());

                    let model = self.state.make_model(ctx.params.model.as_deref());
                    let result = run_agent_loop(
                        &model,
                        &p,
                        ctx,
                        cancel,
                        LoopOpts {
                            extra: None,
                            tool_filter: self.state.tool_filter.as_deref(),
                            classify_error: Some(default_classify as ErrorClassifier),
                        },
                        task_selector,
                    )
                    .await;

                    if let Ok(mut guard) = self.state.cancels.lock() {
                        if let Some(inner) = guard.get_mut(&gremlin_id) {
                            inner.remove(&id);
                            if inner.is_empty() {
                                guard.remove(&gremlin_id);
                            }
                        }
                    }
                    result
                }
            },
        )
        .await
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        let params = {
            let guard = self.state.last_ctx.lock().unwrap();
            let ctx = guard.as_ref().ok_or_else(|| ClientError::Runtime {
                message: "resume() called before run()".into(),
            })?;
            ctx.params.clone()
        };
        self.run(params).await
    }

    fn reap_all(&self, gremlin_id: &str) {
        if let Ok(mut guard) = self.state.cancels.lock() {
            let tokens: Vec<_> = guard
                .remove(gremlin_id)
                .into_iter()
                .flat_map(|m| m.into_values())
                .collect();
            let count = tokens.len();
            log::debug!(
                "{}::reap_all: cancelling {count} in-flight token(s) for gremlin_id={gremlin_id} (model={})",
                self.state.log_label,
                self.state.model,
            );
            for token in &tokens {
                token.cancel();
            }
        }
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::super::agent_loop::CancelToken;
    use super::*;
    use crate::test_support::EnvGuard;

    /// A fake API key that `copilot::Client::builder().api_key(…).build()`
    /// accepts without making network calls.
    const FAKE_API_KEY: &str = "tid=1;exp=9999999999";

    /// Lock the process-state guard and scrub every Copilot credential source.
    fn scrub_copilot_env(guard: &mut EnvGuard) {
        guard.remove("GITHUB_COPILOT_API_KEY");
        guard.remove("COPILOT_API_KEY");
        guard.remove("COPILOT_GITHUB_ACCESS_TOKEN");
        guard.remove("GITHUB_TOKEN");
    }

    /// Set up an isolated sandbox with no providers.json and return the guard.
    fn isolated_env() -> EnvGuard {
        let mut guard = EnvGuard::lock();
        scrub_copilot_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        guard.set("HOME", tmp.path());
        guard
    }

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "copilot");
        assert_eq!(DEFAULT_MODEL, "gpt-4o");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let client = copilot::Client::builder()
            .api_key("tid=1;exp=9999999999")
            .allow_device_flow(false)
            .build()
            .unwrap();
        let backend = CopilotBackend {
            state: CopilotRunState {
                client,
                model: "gpt-4o".into(),
                tool_filter: None,
                intent: None,
                strict_tools: false,
                tool_result_array_content: false,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "test".into(),
            },
        };
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-test".to_string())
            .or_default()
            .insert(1, a.clone());
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-test".to_string())
            .or_default()
            .insert(2, b.clone());
        backend
            .state
            .cancels
            .lock()
            .unwrap()
            .entry("gr-sibling".to_string())
            .or_default()
            .insert(3, sibling.clone());
        backend.reap_all("gr-test");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-test")
            .is_none());
        assert!(!sibling.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-sibling")
            .is_some());
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let _guard = isolated_env();

        let result = CopilotBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("no credentials for provider 'copilot'"),
            "got: {err}"
        );
    }

    // ── auth precedence tests ─────────────────────────────────────────

    #[test]
    fn auth_precedence_api_key_over_github_token() {
        let mut guard = isolated_env();
        guard.set("GITHUB_COPILOT_API_KEY", FAKE_API_KEY);
        guard.set("COPILOT_GITHUB_ACCESS_TOKEN", "ghp_fake_token");

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::GitHubCopilotApiKey,
            "GITHUB_COPILOT_API_KEY should win over COPILOT_GITHUB_ACCESS_TOKEN"
        );
    }

    #[test]
    fn auth_precedence_copilot_api_key_fallback() {
        let mut guard = isolated_env();
        guard.set("COPILOT_API_KEY", FAKE_API_KEY);

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::CopilotApiKey,
            "COPILOT_API_KEY should work as fallback"
        );
    }

    #[test]
    fn auth_precedence_github_token_over_providers_json() {
        let mut guard = isolated_env();
        guard.set("GITHUB_TOKEN", "ghp_fake_token");

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::GitHubToken,
            "GITHUB_TOKEN should win over providers.json"
        );
    }

    #[test]
    fn auth_precedence_copilot_github_access_token_over_github_token() {
        let mut guard = isolated_env();
        guard.set("COPILOT_GITHUB_ACCESS_TOKEN", "ghp_copilot_token");
        guard.set("GITHUB_TOKEN", "ghp_other_token");

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::CopilotGitHubAccessToken,
            "COPILOT_GITHUB_ACCESS_TOKEN should win over GITHUB_TOKEN"
        );
    }

    #[test]
    fn auth_precedence_providers_json_fallback() {
        let mut guard = isolated_env();
        // No env vars set — only providers.json.
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let providers_path = config_dir.join("providers.json");
        std::fs::write(
            &providers_path,
            format!(
                r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}"}}}}"#
            ),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::ProvidersJson,
            "providers.json should be the fallback when no env vars are set"
        );
    }

    #[test]
    fn auth_precedence_providers_json_pat_fallback() {
        let mut guard = isolated_env();
        // No env vars set — only providers.json with a pat field.
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let providers_path = config_dir.join("providers.json");
        std::fs::write(
            &providers_path,
            r#"{"copilot": {"pat": "ghp_fake_pat_token"}}"#,
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::ProvidersJsonPat,
            "providers.json pat field should be the fallback when no env vars are set"
        );
    }

    #[test]
    fn auth_precedence_providers_json_api_key_wins_over_pat() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let providers_path = config_dir.join("providers.json");
        std::fs::write(
            &providers_path,
            format!(
                r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}", "pat": "ghp_fake_pat_token"}}}}"#
            ),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::ProvidersJson,
            "providers.json api-key should win over pat when both are present"
        );
    }

    #[test]
    fn auth_precedence_apps_json_auto_discovery() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();

        // Simulate the Copilot extension's apps.json under $HOME/.config.
        let copilot_config_dir = tmp.path().join(".config").join("github-copilot");
        std::fs::create_dir_all(&copilot_config_dir).unwrap();
        std::fs::write(
            copilot_config_dir.join("apps.json"),
            r#"{"github.com:app-id": {"oauth_token": "ghu_auto_token"}}"#,
        )
        .unwrap();

        // Point $HOME at the temp dir so copilot_oauth_token() finds it.
        guard.set("HOME", tmp.path());
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());

        let (_, source) = resolve_auth().unwrap();
        assert_eq!(
            source,
            CopilotAuthSource::AppsJson,
            "should auto-discover oauth_token from apps.json"
        );
    }

    // ── parse_extra_params tests ─────────────────────────────────────

    #[test]
    fn extra_params_intent_edits() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "edits".into());
        let (intent, _, _) = parse_extra_params(&ep);
        assert_eq!(intent, Some(CopilotIntent::Edits));
    }

    #[test]
    fn extra_params_intent_defaults_to_panel() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "bogus".into());
        let (intent, _, _) = parse_extra_params(&ep);
        assert_eq!(intent, Some(CopilotIntent::Panel));
    }

    #[test]
    fn extra_params_intent_missing_is_none() {
        let (intent, _, _) = parse_extra_params(&indexmap::IndexMap::new());
        assert_eq!(intent, None);
    }

    #[test]
    fn extra_params_strict_tools_true() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "true".into());
        let (_, strict_tools, _) = parse_extra_params(&ep);
        assert!(strict_tools);
    }

    #[test]
    fn extra_params_strict_tools_one() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "1".into());
        let (_, strict_tools, _) = parse_extra_params(&ep);
        assert!(strict_tools);
    }

    #[test]
    fn extra_params_strict_tools_false_by_default() {
        let (_, strict_tools, _) = parse_extra_params(&indexmap::IndexMap::new());
        assert!(!strict_tools);
    }

    #[test]
    fn extra_params_tool_result_array_content_true() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "true".into());
        let (_, _, tool_result_array_content) = parse_extra_params(&ep);
        assert!(tool_result_array_content);
    }

    #[test]
    fn extra_params_tool_result_array_content_one() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "1".into());
        let (_, _, tool_result_array_content) = parse_extra_params(&ep);
        assert!(tool_result_array_content);
    }

    #[test]
    fn extra_params_tool_result_array_content_false_by_default() {
        let (_, _, tool_result_array_content) =
            parse_extra_params(&indexmap::IndexMap::new());
        assert!(!tool_result_array_content);
    }
}