use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::completion::CompletionError;
use rig_core::providers::openai;

use super::agent_loop::{CancelToken, ErrorClassifier, RunContext};
use super::backend::{Backend, ClientError, RunParams};
use super::openai_backend::{build_extra_params, run_with_agent_loop};
use super::protocol::CompletedRun;
use super::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use super::stream;

const TRANSIENT_SUBSTRINGS: &[&str] = &[
    "capacity",
    "rate limit",
    "too many requests",
    "try again",
    "please retry",
    "server error",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "overloaded",
    "timed out in queue",
    " 529",
    "upstream",
    "provider_error",
    "error decoding response body",
    "connection reset",
    "connection refused",
    "dns error",
    "tls handshake",
];

fn classify_openrouter_error(err: CompletionError) -> ClientError {
    // Phase 1: status codes (same as default).
    if let Some(status) = err.provider_response_status() {
        let code = status.as_u16();
        if (500..600).contains(&code) || code == 429 {
            return ClientError::ApiServerError {
                message: err.to_string(),
            };
        }
        // 4xx — fall through to Phase 2.
    } else {
        // No HTTP status — mid-stream SSE drop.
        log::warn!("retrying provider error (no HTTP status): {}", err);
        return ClientError::ApiServerError {
            message: err.to_string(),
        };
    }

    // Phase 2: substring backstop for 4xx.
    let body = err.to_string().to_lowercase();
    if body_contains_transient(&body) {
        log::warn!(
            "retrying provider error (OpenRouter substring match): {}",
            err
        );
        return ClientError::ApiServerError {
            message: err.to_string(),
        };
    }

    ClientError::Runtime {
        message: err.to_string(),
    }
}

fn body_contains_transient(body: &str) -> bool {
    TRANSIENT_SUBSTRINGS.iter().any(|kw| body.contains(kw))
}

pub struct OpenRouterBackend {
    client: openai::CompletionsClient,
    model: String,
    tool_filter: Option<Vec<String>>,
    client_params: HashMap<String, String>,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<u64, Arc<CancelToken>>>,
    next_id: AtomicU64,
}

impl OpenRouterBackend {
    pub fn new(
        client: openai::CompletionsClient,
        model: String,
        tool_filter: Option<Vec<String>>,
        client_params: HashMap<String, String>,
    ) -> Self {
        let model = if model.is_empty() {
            "gpt-4o".to_string()
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
        let classify: ErrorClassifier = Arc::new(classify_openrouter_error);
        run_with_agent_loop(
            &self.client,
            &model_name,
            prompt,
            ctx,
            cancel,
            self.extra_params(),
            self.tool_filter.as_deref(),
            Some(classify),
        )
        .await
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
impl Backend for OpenRouterBackend {
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
            reminder_budget: crate::config::artifact_reminder_budget(),
            completion_nudge_budget: crate::config::completion_nudge_budget(),
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
    use http::StatusCode;

    #[test]
    fn phase1_5xx_retryable() {
        let err = CompletionError::from_http_response(StatusCode::SERVICE_UNAVAILABLE, "boom");
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn phase1_429_retryable() {
        let err = CompletionError::from_http_response(StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn phase1_no_http_status_retryable() {
        let err = CompletionError::ProviderError("something broke".into());
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn phase2_server_error_match() {
        let err = CompletionError::from_http_response(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"upstream server error","code":"server_error"}}"#,
        );
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn phase2_upstream_match() {
        let err = CompletionError::from_http_response(
            StatusCode::BAD_REQUEST,
            "upstream provider failure",
        );
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn phase2_provider_error_match() {
        let err = CompletionError::from_http_response(
            StatusCode::BAD_REQUEST,
            r#"{"error":"provider_error: model overloaded"}"#,
        );
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::ApiServerError { .. }
        ));
    }

    #[test]
    fn phase2_no_match_fatal() {
        let err = CompletionError::from_http_response(
            StatusCode::BAD_REQUEST,
            "invalid request: missing required field",
        );
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::Runtime { .. }
        ));
    }

    #[test]
    fn phase2_401_no_match_fatal() {
        let err = CompletionError::from_http_response(StatusCode::UNAUTHORIZED, "bad key");
        assert!(matches!(
            classify_openrouter_error(err),
            ClientError::Runtime { .. }
        ));
    }
}
