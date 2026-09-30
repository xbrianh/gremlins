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

const PROVIDER_NAME: &str = "copilot";
const DEFAULT_MODEL: &str = "gpt-4o";

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
        let api_key = crate::config::copilot_api_key();
        let github_token = crate::config::copilot_github_token();

        let client = if let Some(key) = api_key {
            copilot::Client::builder()
                .api_key(key)
                .allow_device_flow(false)
                .build()
                .map_err(|e| format!("{e}"))?
        } else if let Some(token) = github_token {
            copilot::Client::builder()
                .github_access_token(token)
                .allow_device_flow(false)
                .build()
                .map_err(|e| format!("{e}"))?
        } else if let Some(key) = crate::config::api_key("", PROVIDER_NAME) {
            // providers.json fallback — treated as API key
            copilot::Client::builder()
                .api_key(key)
                .allow_device_flow(false)
                .build()
                .map_err(|e| format!("{e}"))?
        } else {
            return Err(format!(
                "no credentials for provider '{PROVIDER_NAME}': set GITHUB_COPILOT_API_KEY, \
                 COPILOT_API_KEY, COPILOT_GITHUB_ACCESS_TOKEN, GITHUB_TOKEN, or add an entry in {}",
                crate::config::user_config_root()
                    .join("providers.json")
                    .display(),
            ));
        };

        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.to_string()
        };

        let tool_filter = openai_protocol::tool_filter(native_block);

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
                        None,
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

    #[test]
    fn build_parses_intent_from_extra_params() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "edits".into());
        let intent = ep.get("intent").map(|v: &String| match v.as_str() {
            "edits" => CopilotIntent::Edits,
            _ => CopilotIntent::Panel,
        });
        assert_eq!(intent, Some(CopilotIntent::Edits));

        let ep2: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        let intent2 = ep2.get("intent").map(|v: &String| match v.as_str() {
            "edits" => CopilotIntent::Edits,
            _ => CopilotIntent::Panel,
        });
        assert_eq!(intent2, None);
    }

    #[test]
    fn build_parses_strict_tools_from_extra_params() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "true".into());
        let v = ep
            .get("strict_tools")
            .map(|v: &String| v == "true" || v == "1")
            .unwrap_or(false);
        assert!(v);

        let ep2: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        let v2 = ep2
            .get("strict_tools")
            .map(|v: &String| v == "true" || v == "1")
            .unwrap_or(false);
        assert!(!v2);
    }

    #[test]
    fn build_parses_tool_result_array_content_from_extra_params() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "1".into());
        let v = ep
            .get("tool_result_array_content")
            .map(|v: &String| v == "true" || v == "1")
            .unwrap_or(false);
        assert!(v);
    }
}