use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::providers::copilot::{Copilot, CopilotConfig, CopilotIntent};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::openai_protocol;
use crate::clients::protocol::CompletedRun;
use crate::clients::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use crate::clients::task::TaskModelSelector;

const PROVIDER_NAME: &str = "copilot";
const DEFAULT_MODEL: &str = "gpt-4o";

/// Resolve credentials and build a Copilot client.
/// OAuth device-code flow is disabled — gremlins run unattended.
///
/// Auth precedence: `GITHUB_COPILOT_API_KEY` → `COPILOT_API_KEY` →
/// `providers.yaml` `"copilot"` entry (`api-key`).
fn resolve_auth() -> Result<Copilot, String> {
    if let Some(key) = crate::config::copilot_api_key() {
        return Ok(CopilotConfig::new(key).client());
    }
    if let Some(key) = crate::clients::config::api_key("", PROVIDER_NAME) {
        return Ok(CopilotConfig::new(key).client());
    }
    Err(format!(
        "no API key for provider '{PROVIDER_NAME}': set GITHUB_COPILOT_API_KEY, \
         COPILOT_API_KEY, or add an entry with \"api-key\" in {}",
        crate::config::user_config_root()
            .join("providers.yaml")
            .display(),
    ))
}

// ── CopilotRunState ──────────────────────────────────────────────────────

struct CopilotRunState {
    client: Copilot,
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
    fn make_model(&self, override_model: Option<&str>) -> DynModel<Completion> {
        let model_name = match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        };
        let mut mdl = self.client.completion(&model_name);
        if let Some(intent) = self.intent {
            mdl.wire = mdl.wire.with_intent(intent);
        }
        if self.strict_tools {
            mdl.wire = mdl.wire.with_strict_tools();
        }
        if self.tool_result_array_content {
            mdl.wire = mdl.wire.with_tool_result_array_content();
        }
        mdl.erase()
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
    /// `providers.yaml` `"copilot"` entry → error.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let client = resolve_auth()?;

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
/// `settings.yaml` declares no `task-clients` entries this backend can serve.
fn copilot_task_model_selector(
    client: &Copilot,
    task_clients_exact: &HashMap<String, String>,
    task_clients_prefix: &HashMap<String, String>,
) -> Option<TaskModelSelector<DynModel<Completion>>> {
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
                Some(client.completion(model).erase())
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
    async fn run(
        &self,
        params: RunParams,
        mut interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        validate_max_retries(params.max_retries).map_err(|m| ClientError::Runtime { message: m })?;

        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };
        let ctx = RunContext {
            params: params.clone(),
            prefix: prefix.clone(),
            idle_timeout,
            expected_artifact_paths: params.expected_artifact_paths.clone(),
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
        };
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
                // Move interactive on first attempt; subsequent retries get None.
                let interactive = interactive.take();
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
                        model,
                        &p,
                        ctx,
                        cancel,
                        LoopOpts {
                            extra: None,
                            tool_filter: self.state.tool_filter.as_deref(),
                            classify_error: Some(default_classify as ErrorClassifier),
                            max_tokens: None,
                        },
                        task_selector,
                        interactive,
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
        self.run(params, None).await
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

    const FAKE_API_KEY: &str = "tid=1;exp=9999999999";

    fn scrub_copilot_env(guard: &mut EnvGuard) {
        guard.remove("GITHUB_COPILOT_API_KEY");
        guard.remove("COPILOT_API_KEY");
        guard.remove("XDG_CONFIG_HOME");
    }

    /// Set up an isolated sandbox with no providers.yaml and return the guard.
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
        let client = Copilot::new("tid=1;exp=9999999999");
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
            err.contains("no API key for provider") || err.contains("no credentials for provider"),
            "got: {err}"
        );
    }

    // ── auth tests ───────────────────────────────────────────────────

    #[test]
    fn auth_github_copilot_api_key() {
        let mut guard = isolated_env();
        guard.set("GITHUB_COPILOT_API_KEY", FAKE_API_KEY);
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_copilot_api_key_fallback() {
        let mut guard = isolated_env();
        guard.set("COPILOT_API_KEY", FAKE_API_KEY);
        assert!(resolve_auth().is_ok());
    }

    #[test]
    fn auth_providers_yaml() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            format!(r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}"}}}}"#),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        assert!(resolve_auth().is_ok());
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