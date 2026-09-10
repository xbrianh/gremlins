use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::providers::openai;

use super::agent_loop::{run_agent_loop, CancelToken, LoopOpts, RunContext};
use super::backend::{Backend, ClientError, RunParams};
use super::protocol::CompletedRun;
use super::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use super::stream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiProvider {
    OpenAi,
    Xai,
    OpenRouter,
}

impl OpenAiProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Xai => "xai",
            Self::OpenRouter => "openrouter",
        }
    }

    pub fn api_key_env(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Xai => "XAI_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Xai => "https://api.x.ai/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi | Self::OpenRouter => "gpt-4o",
            Self::Xai => "grok-4",
        }
    }
}

pub struct OpenAiBackend {
    client: openai::CompletionsClient,
    model: String,
    tool_filter: Option<Vec<String>>,
    client_params: HashMap<String, String>,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<u64, Arc<CancelToken>>>,
    next_id: AtomicU64,
}

impl OpenAiBackend {
    pub fn new(
        provider: OpenAiProvider,
        client: openai::CompletionsClient,
        model: String,
        tool_filter: Option<Vec<String>>,
        client_params: HashMap<String, String>,
    ) -> Self {
        let model = if model.is_empty() {
            provider.default_model().to_string()
        } else {
            model
        };
        Self {
            client,
            model,
            tool_filter,
            client_params,
            last_ctx: Mutex::new(None),
            cancels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn extra_params(&self) -> Option<serde_json::Value> {
        build_extra_params(&self.client_params)
    }

    fn effective_model(&self, override_model: Option<&str>) -> String {
        match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        }
    }

    async fn attempt(&self, prompt: &str, ctx: &RunContext) -> Result<CompletedRun, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = CancelToken::new();
        self.cancels.lock().unwrap().insert(id, cancel.clone());
        let result = self.attempt_inner(prompt, ctx, cancel).await;
        self.cancels.lock().unwrap().remove(&id);
        result
    }

    async fn attempt_inner(
        &self,
        prompt: &str,
        ctx: &RunContext,
        cancel: Arc<CancelToken>,
    ) -> Result<CompletedRun, ClientError> {
        let model_name = self.effective_model(ctx.params.model.as_deref());
        let model = self.client.completion_model(&model_name);
        let mut ctx = ctx.clone();
        ctx.params.model = Some(model_name);
        run_agent_loop(
            &model,
            prompt,
            &ctx,
            cancel,
            LoopOpts {
                extra: self.extra_params(),
                tool_filter: self.tool_filter.as_deref(),
            },
        )
        .await
    }
}

fn build_extra_params(client_params: &HashMap<String, String>) -> Option<serde_json::Value> {
    let mut params = serde_json::Map::new();

    params.insert("parallel_tool_calls".into(), serde_json::Value::Bool(true));

    // reasoning effort: client param > env var
    let effort = client_params
        .get("reasoning")
        .cloned()
        .or_else(crate::config::reasoning_effort);
    if let Some(effort) = effort {
        params.insert(
            "reasoning".into(),
            serde_json::json!({"effort": effort, "summary": "auto"}),
        );
    }

    // Pass through any other client params. Parse each value as JSON so
    // numbers/bools survive as their natural types; fall back to a plain
    // string if the value isn't valid JSON (e.g. an opaque enum like
    // thinking=deepseek).
    // "reasoning" and "parallel_tool_calls" are excluded — reserved keys with
    // provider-specific handling above.
    for (k, v) in client_params {
        if k != "reasoning" && k != "parallel_tool_calls" {
            let val = match serde_json::from_str::<serde_json::Value>(v) {
                Ok(parsed) => parsed,
                Err(_) => serde_json::Value::String(v.clone()),
            };
            params.insert(k.clone(), val);
        }
    }

    if params.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(params))
    }
}

fn classify_retryable(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Timeout { .. } | ClientError::ApiServerError { .. }
    )
}

fn retry_prompt(err: &ClientError, prompt: &str, on_timeout_prompt: Option<&str>) -> String {
    match err {
        ClientError::Timeout { .. } => on_timeout_prompt.unwrap_or(prompt).to_string(),
        _ => prompt.to_string(),
    }
}

#[async_trait]
impl Backend for OpenAiBackend {
    async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError> {
        validate_max_retries(params.max_retries)
            .map_err(|m| ClientError::Runtime { message: m })?;

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
            reminder_budget: params.artifact_reminder_count,
        };
        *self.last_ctx.lock().unwrap() = Some(ctx.clone());

        let prompt = Mutex::new(params.prompt.clone());
        let timeout_prompt = params.on_timeout_prompt.clone();
        let backoff = &STREAM_IDLE_BACKOFF[..params.max_retries];

        retry::with_retry(
            backoff,
            classify_retryable,
            |attempt, e, wait| {
                let next = retry_prompt(e, &prompt.lock().unwrap(), timeout_prompt.as_deref());
                *prompt.lock().unwrap() = next;
                let cause = match e {
                    ClientError::Timeout { .. } => "stream idle timeout",
                    ClientError::ApiServerError { .. } => "transient-error",
                    _ => "error",
                };
                eprintln!(
                    "{} {}stream {cause}, retrying in {wait}s ({}/{})...",
                    stream::ts_internal(),
                    prefix,
                    attempt + 1,
                    params.max_retries
                );
            },
            || {
                let p = prompt.lock().unwrap().clone();
                let ctx = ctx.clone();
                async move { self.attempt(&p, &ctx).await }
            },
        )
        .await
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        let params = {
            let guard = self.last_ctx.lock().unwrap();
            let ctx = guard.as_ref().ok_or_else(|| ClientError::Runtime {
                message: "resume() called before run()".into(),
            })?;
            ctx.params.clone()
        };
        self.run(params).await
    }

    fn reap_all(&self) {
        if let Ok(guard) = self.cancels.lock() {
            for token in guard.values() {
                token.cancel();
            }
        }
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::agent_loop::map_stream_error;

    #[test]
    fn provider_identity() {
        assert_eq!(OpenAiProvider::OpenAi.name(), "openai");
        assert_eq!(OpenAiProvider::Xai.name(), "xai");
        assert_eq!(OpenAiProvider::OpenRouter.name(), "openrouter");
        assert_eq!(OpenAiProvider::OpenAi.api_key_env(), "OPENAI_API_KEY");
        assert_eq!(OpenAiProvider::Xai.api_key_env(), "XAI_API_KEY");
        assert_eq!(
            OpenAiProvider::OpenRouter.api_key_env(),
            "OPENROUTER_API_KEY"
        );
        assert_eq!(OpenAiProvider::Xai.base_url(), "https://api.x.ai/v1");
        assert_eq!(
            OpenAiProvider::OpenRouter.base_url(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(OpenAiProvider::Xai.default_model(), "grok-4");
    }

    #[test]
    fn extra_params_default_no_reasoning() {
        let p = build_extra_params(&HashMap::new()).unwrap();
        assert_eq!(p["parallel_tool_calls"], true);
        assert!(p.get("reasoning").is_none());
    }

    #[test]
    fn extra_params_client_reasoning_overrides() {
        let mut cp = HashMap::new();
        cp.insert("reasoning".into(), "low".into());
        let p = build_extra_params(&cp).unwrap();
        assert_eq!(p["reasoning"]["effort"], "low");
        assert_eq!(p["reasoning"]["summary"], "auto");
    }

    /// Lock the contract between [`build_extra_params`] output shape and the
    /// `opts.extra -> reasoning.effort` extraction in [`run_agent_loop`]. If
    /// the JSON structure produced by `build_extra_params` ever changes, this
    /// test must also change — preventing silent `reasoning_effort=default`
    /// degradation in stage-init telemetry.
    #[test]
    fn build_extra_params_to_reasoning_effort_extraction_locked() {
        let mut cp = HashMap::new();
        cp.insert("reasoning".into(), "low".into());
        let extra = build_extra_params(&cp).unwrap();
        let effort = extra
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|e| e.as_str());
        assert_eq!(effort, Some("low"));

        // Without reasoning, extraction returns None
        let extra = build_extra_params(&HashMap::new()).unwrap();
        let effort = extra
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(|e| e.as_str());
        assert_eq!(effort, None);
    }

    #[test]
    fn extra_params_client_passthrough() {
        let mut cp = HashMap::new();
        cp.insert("thinking".into(), "deepseek".into());
        cp.insert("foo".into(), "bar".into());
        let p = build_extra_params(&cp).unwrap();
        assert_eq!(p["thinking"], "deepseek");
        assert_eq!(p["foo"], "bar");
        assert!(p.get("reasoning").is_none());
    }

    #[test]
    fn extra_params_client_passthrough_json_types() {
        let mut cp = HashMap::new();
        cp.insert("temperature".into(), "0.7".into());
        cp.insert("top_p".into(), "0.95".into());
        cp.insert("stream".into(), "true".into());
        cp.insert("max_tokens".into(), "4096".into());
        cp.insert("stop".into(), "[\"END\"]".into()); // JSON array
        let p = build_extra_params(&cp).unwrap();
        // numbers
        assert_eq!(p["temperature"], serde_json::json!(0.7));
        assert_eq!(p["top_p"], serde_json::json!(0.95));
        assert_eq!(p["max_tokens"], serde_json::json!(4096));
        // bool
        assert_eq!(p["stream"], serde_json::json!(true));
        // JSON array passthrough
        assert_eq!(p["stop"], serde_json::json!(["END"]));
    }

    #[test]
    fn extra_params_client_reasoning_plus_passthrough() {
        let mut cp = HashMap::new();
        cp.insert("reasoning".into(), "high".into());
        cp.insert("thinking".into(), "deepseek".into());
        let p = build_extra_params(&cp).unwrap();
        assert_eq!(p["reasoning"]["effort"], "high");
        assert_eq!(p["thinking"], "deepseek");
        // xai auto-inserts parallel_tool_calls alongside reasoning + passthrough
        assert_eq!(p["parallel_tool_calls"], true);
    }

    #[test]
    fn transient_classifier() {
        assert!(retry::is_transient_stream_error(
            "The model is currently at capacity"
        ));
        assert!(retry::is_transient_stream_error("rate limit exceeded"));
        assert!(!retry::is_transient_stream_error("Invalid API key"));
        assert!(matches!(
            map_stream_error("rate limit exceeded".into()),
            ClientError::ApiServerError { .. }
        ));
        assert!(matches!(
            map_stream_error("Invalid API key".into()),
            ClientError::Runtime { .. }
        ));
        assert!(retry::is_transient_stream_error(
            "Http client error: error sending request for url (https://openrouter.ai/v1/chat/completions)"
        ));
        assert!(retry::is_transient_stream_error(
            "Http client error: error decoding response body"
        ));
        assert!(matches!(
            map_stream_error("Http client error: connection reset".into()),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn retry_prompt_swaps_only_on_timeout() {
        let timeout = ClientError::Timeout {
            message: "idle".into(),
        };
        let api = ClientError::ApiServerError {
            message: "rate limit".into(),
        };
        assert_eq!(
            retry_prompt(&timeout, "orig", Some("timeout-prompt")),
            "timeout-prompt"
        );
        assert_eq!(retry_prompt(&api, "orig", Some("timeout-prompt")), "orig");
        assert_eq!(retry_prompt(&timeout, "orig", None), "orig");
    }

    #[test]
    fn reap_all_cancels_every_token() {
        let client = openai::Client::builder()
            .api_key(rig_core::client::BearerAuth::from("sk-test"))
            .base_url("https://api.openai.com/v1")
            .build()
            .unwrap()
            .completions_api();
        let backend = OpenAiBackend::new(
            OpenAiProvider::OpenAi,
            client,
            "gpt-4o".into(),
            None,
            HashMap::new(),
        );
        let a = CancelToken::new();
        let b = CancelToken::new();
        backend.cancels.lock().unwrap().insert(1, a.clone());
        backend.cancels.lock().unwrap().insert(2, b.clone());
        backend.reap_all();
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
    }
}
