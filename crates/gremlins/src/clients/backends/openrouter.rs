use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rig_core::completion::CompletionError;

use crate::clients::agent_loop::ErrorClassifier;
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::openai_protocol::{self, reap_openai_compat, run_openai_compat, OpenAiRunState};
use crate::clients::protocol::CompletedRun;

/// Base URL for OpenRouter's OpenAI-compatible API.
const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Provider name this backend answers to, used to match `task-clients` specs.
const PROVIDER_NAME: &str = "openrouter";

const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

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
    let message = err.to_string();
    // Phase 1: status codes (same as default).
    if let Some(status) = err.provider_response_status() {
        let code = status.as_u16();
        if (500..600).contains(&code) || code == 429 {
            return ClientError::ApiServerError { message };
        }
        // Only 4xx falls through to Phase 2; anything else is fatal.
        if !(400..500).contains(&code) {
            return ClientError::Runtime { message };
        }
    } else {
        // No HTTP status — mid-stream SSE drop.
        log::warn!("retrying provider error (no HTTP status): {}", err);
        return ClientError::ApiServerError { message };
    }

    // Phase 2: substring backstop for 4xx.
    if body_contains_transient(&message.to_lowercase()) {
        log::warn!(
            "retrying provider error (OpenRouter substring match): {}",
            err
        );
        return ClientError::ApiServerError { message };
    }

    ClientError::Runtime { message }
}

fn body_contains_transient(body: &str) -> bool {
    TRANSIENT_SUBSTRINGS.iter().any(|kw| body.contains(kw))
}

pub struct OpenRouterBackend {
    state: OpenAiRunState,
}

impl OpenRouterBackend {
    pub fn new(
        client: rig_core::providers::openai::CompletionsClient,
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
            state: OpenAiRunState::new(
                client,
                model,
                tool_filter,
                client_params,
                "OpenRouterBackend".to_string(),
            ),
        }
    }

    /// Build an OpenRouter backend. Resolves `OPENROUTER_API_KEY` → `providers.json`.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let key =
            crate::config::api_key(API_KEY_ENV, PROVIDER_NAME).ok_or_else(|| {
                format!(
                    "no API key for provider '{PROVIDER_NAME}': set {API_KEY_ENV} or add an entry in {}",
                    crate::config::user_config_root()
                        .join("providers.json")
                        .display(),
                )
            })?;
        let client = openai_protocol::build_openai_client(&key, BASE_URL)?;
        let model = if model.is_empty() {
            "gpt-4o".to_string()
        } else {
            model.to_string()
        };
        Ok(Arc::new(Self::new(
            client,
            model,
            openai_protocol::tool_filter(native_block),
            openai_protocol::string_map(extra_params),
        )))
    }
}

#[async_trait]
impl Backend for OpenRouterBackend {
    async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError> {
        let classify: ErrorClassifier = classify_openrouter_error;
        run_openai_compat(&self.state, params, Some(classify), PROVIDER_NAME).await
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
        reap_openai_compat(&self.state, gremlin_id);
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
    fn phase2_non_4xx_never_retryable() {
        // A 2xx/3xx whose body contains a transient keyword must not be retried.
        for status in [StatusCode::OK, StatusCode::MOVED_PERMANENTLY] {
            let err = CompletionError::from_http_response(status, "upstream server error");
            assert!(matches!(
                classify_openrouter_error(err),
                ClientError::Runtime { .. }
            ));
        }
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