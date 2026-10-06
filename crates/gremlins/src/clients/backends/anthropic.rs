use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::providers::anthropic;

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::openai_protocol;
use crate::clients::protocol::CompletedRun;
use crate::clients::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use crate::clients::token_provider::{self, TokenProvider};
use crate::clients::config::ProviderAuth;
use rig_core::http_client::ReqwestClient;

// ── AnthropicClientState ─────────────────────────────────────────────────

/// Either a statically-built client (for ApiKey / Token auth) or the
/// ingredients to build one dynamically per attempt (for identity-based auth).
enum AnthropicClientState {
    Static(anthropic::Client),
    Dynamic {
        token_provider: Box<dyn TokenProvider>,
        base_url: String,
        http_client: ReqwestClient,
    },
}

// ── AnthropicRunState ────────────────────────────────────────────────────

struct AnthropicRunState {
    client_state: AnthropicClientState,
    model: String,
    tool_filter: Option<Vec<String>>,
    client_params: HashMap<String, String>,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    next_id: AtomicU64,
    log_label: String,
}

impl AnthropicRunState {
    fn extra_params(&self) -> Option<serde_json::Value> {
        build_anthropic_extra_params(&self.client_params)
    }

    fn effective_model(&self, override_model: Option<&str>) -> String {
        match override_model {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => self.model.clone(),
        }
    }

    /// Build a [`RunContext`] from [`RunParams`] and stash it in `last_ctx` for resume.
    fn prepare_context(&self, params: RunParams) -> RunContext {
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
        ctx
    }

    /// Execute a single agent-loop attempt, registering and cleaning up a
    /// cancel token for the given `gremlin_id`.
    async fn single_attempt(
        &self,
        prompt: &str,
        ctx: RunContext,
        cancel: Arc<CancelToken>,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        let gremlin_id = ctx.params.gremlin_id.clone().unwrap_or_default();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // Check+insert under the cancels lock so reap_all cannot
        // remove the gremlin entry between the check and insert.
        {
            let mut guard = self.cancels.lock().unwrap();
            if cancel.is_cancelled() {
                return Err(ClientError::Runtime {
                    message: "cancelled".into(),
                });
            }
            guard
                .entry(gremlin_id.clone())
                .or_default()
                .insert(id, cancel.clone());
        }

        let model_name = self.effective_model(ctx.params.model.as_deref());

        // For static auth we use the pre-built client; for dynamic auth
        // we acquire a fresh token and build a client per attempt.
        let result = match &self.client_state {
            AnthropicClientState::Static(client) => {
                let model = client.completion_model(&model_name);
                run_agent_loop(
                    &model,
                    prompt,
                    ctx,
                    cancel,
                    LoopOpts {
                        extra: self.extra_params(),
                        tool_filter: self.tool_filter.as_deref(),
                        classify_error: Some(default_classify as ErrorClassifier),
                    },
                    None,
                    interactive,
                )
                .await
            }
            AnthropicClientState::Dynamic {
                token_provider,
                base_url,
                http_client,
            } => {
                // Capture errors into a local so execution always flows
                // through the cancellation-map cleanup below.
                let dyn_result = async {
                    let scope = crate::clients::config::azure_auth_scope(
                        "ANTHROPIC_AUTH_SCOPE",
                        "anthropic",
                        "https://cognitiveservices.azure.com/.default",
                    );
                    let token = token_provider
                        .get_token(&scope)
                        .await
                        .map_err(|e| ClientError::Runtime {
                            message: format!("Anthropic token acquisition failed: {e}"),
                        })?;
                    let client = anthropic::Client::builder()
                        .api_key(anthropic::client::AnthropicKey::from(token))
                        .base_url(base_url)
                        .http_client(http_client.clone())
                        .build()
                        .map_err(|e| ClientError::Runtime {
                            message: format!("failed to build Anthropic client: {e}"),
                        })?;
                    let model = client.completion_model(&model_name);
                    run_agent_loop(
                        &model,
                        prompt,
                        ctx,
                        cancel,
                        LoopOpts {
                            extra: self.extra_params(),
                            tool_filter: self.tool_filter.as_deref(),
                            classify_error: Some(default_classify as ErrorClassifier),
                        },
                        None,
                        interactive,
                    )
                    .await
                }
                .await;
                dyn_result
            }
        };

        if let Ok(mut guard) = self.cancels.lock() {
            if let Some(inner) = guard.get_mut(&gremlin_id) {
                inner.remove(&id);
                if inner.is_empty() {
                    guard.remove(&gremlin_id);
                }
            }
        }
        result
    }
}

// ── AnthropicBackend ─────────────────────────────────────────────────────

pub struct AnthropicBackend {
    state: AnthropicRunState,
}

impl AnthropicBackend {
    /// Build an Anthropic backend.
    ///
    /// Auth is resolved via [`crate::clients::config::azure_auth_method`]:
    ///
    /// | `anthropic.azure.auth` / `ANTHROPIC_AUTH` | Behaviour |
    /// |---|---|
    /// | (unset) | Static fallback: `anthropic.token` → `anthropic.api-key` |
    /// | `"client-secret"` | Service principal via `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` / `AZURE_TENANT_ID` |
    /// | `"cli"` | `az account get-access-token` |
    /// | `"managed-identity"` | Azure IMDS endpoint |
    /// | `"default"` | Chains client-secret → CLI → managed identity |
    ///
    /// For dynamic methods the client is built per attempt (token acquisition
    /// is async).  Configuration errors (bad env vars) surface at first use.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let base_url = crate::clients::config::base_url(
            "ANTHROPIC_BASE_URL",
            "anthropic",
            "https://api.anthropic.com",
        );

        let auth_method = crate::clients::config::azure_auth_method("ANTHROPIC_AUTH", "anthropic", "ANTHROPIC_TOKEN", "ANTHROPIC_API_KEY")?;

        let model = if model.is_empty() {
            "claude-sonnet-4-6".to_string()
        } else {
            model.to_string()
        };

        let tool_filter = openai_protocol::tool_filter(native_block);
        let client_params = openai_protocol::string_map(extra_params);

        let http_client = ReqwestClient::builder()
            .build()
            .map_err(|e| format!("failed to create HTTP client: {e}"))?;

        let client_state = match auth_method {
            ProviderAuth::ApiKey(key) => {
                let client = anthropic::Client::builder()
                    .api_key(anthropic::client::AnthropicKey::from(key))
                    .base_url(&base_url)
                    .http_client(http_client.clone())
                    .build()
                    .map_err(|e| format!("failed to build Anthropic client: {e}"))?;
                AnthropicClientState::Static(client)
            }
            ProviderAuth::Token(token) => {
                let client = anthropic::Client::builder()
                    .api_key(anthropic::client::AnthropicKey::from(token))
                    .base_url(&base_url)
                    .http_client(http_client.clone())
                    .build()
                    .map_err(|e| format!("failed to build Anthropic client: {e}"))?;
                AnthropicClientState::Static(client)
            }
            ProviderAuth::ClientSecret => AnthropicClientState::Dynamic {
                token_provider: Box::new(token_provider::ClientSecretProvider::new()),
                base_url,
                http_client: http_client.clone(),
            },
            ProviderAuth::Cli => AnthropicClientState::Dynamic {
                token_provider: Box::new(token_provider::AzureCliProvider::new()),
                base_url,
                http_client: http_client.clone(),
            },
            ProviderAuth::ManagedIdentity => AnthropicClientState::Dynamic {
                token_provider: Box::new(token_provider::ManagedIdentityProvider::new()),
                base_url,
                http_client: http_client.clone(),
            },
            ProviderAuth::DefaultAzure => AnthropicClientState::Dynamic {
                token_provider: Box::new(token_provider::DefaultAzureProvider::new()),
                base_url,
                http_client: http_client.clone(),
            },
        };

        Ok(Arc::new(Self {
            state: AnthropicRunState {
                client_state,
                model,
                tool_filter,
                client_params,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "AnthropicBackend".to_string(),
            },
        }))
    }
}

#[async_trait]
impl Backend for AnthropicBackend {
    async fn run(
        &self,
        params: RunParams,
        mut interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        validate_max_retries(params.max_retries).map_err(|m| ClientError::Runtime { message: m })?;

        let ctx = self.state.prepare_context(params.clone());
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
                    "{}stream {cause}, retrying in {wait}s ({}/{})...",
                    ctx.prefix,
                    attempt + 1,
                    params.max_retries
                );
            },
            || {
                let p = prompt.lock().unwrap().clone();
                let ctx = ctx.clone();
                let cancel = cancel.clone();
                // Move interactive on first attempt; subsequent retries get None.
                let interactive = interactive.take();
                async move { self.state.single_attempt(&p, ctx, cancel, interactive).await }
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

// ── Anthropic-specific extra params ────────────────────────────────────

/// Build the `extra` JSON blob for Anthropic requests.
///
/// Unlike the OpenAI-protocol helper, this does **not** inject
/// `parallel_tool_calls` (Anthropic controls parallelism via
/// `tool_choice.disable_parallel_tool_use`) or the OpenAI `reasoning`
/// object.  Only passthrough client params are forwarded.
fn build_anthropic_extra_params(
    client_params: &HashMap<String, String>,
) -> Option<serde_json::Value> {
    let mut params = serde_json::Map::new();

    for (k, v) in client_params {
        let val = match serde_json::from_str::<serde_json::Value>(v) {
            Ok(parsed) => parsed,
            Err(_) => serde_json::Value::String(v.clone()),
        };
        params.insert(k.clone(), val);
    }

    if params.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(params))
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::super::agent_loop::CancelToken;
    use super::*;
    use crate::test_support::EnvGuard;

    fn scrub_anthropic_env(guard: &mut EnvGuard) {
        guard.remove("ANTHROPIC_API_KEY");
        guard.remove("ANTHROPIC_BASE_URL");
        guard.remove("ANTHROPIC_AUTH");
        guard.remove("ANTHROPIC_AUTH_SCOPE");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        guard.remove("AZURE_TENANT_ID");
    }

    fn isolated_env() -> EnvGuard {
        let mut guard = EnvGuard::lock();
        scrub_anthropic_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap().keep();
        guard.set("GREMLINS_SANDBOX_ROOT", &tmp);
        guard.set("HOME", &tmp);
        guard
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let _guard = isolated_env();

        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("no credentials for provider"),
            "got: {err}"
        );
    }

    #[test]
    fn build_with_api_key() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok(), "build should succeed with API key");
    }

    #[test]
    fn build_with_custom_base_url() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");
        guard.set("ANTHROPIC_BASE_URL", "https://anthropic-proxy.example.com");

        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok(), "build should succeed with custom base URL");
    }

    #[test]
    fn build_default_model() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let backend = AnthropicBackend::build(
            "",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        // We can't inspect the model directly through the trait, but we can
        // verify the build succeeded with an empty model string.
        drop(backend);
    }

    #[test]
    fn build_with_auth_client_secret() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH", "client-secret");
        // ClientSecretProvider::new() defers env-var validation to first
        // get_token() call, so build() succeeds even without env vars.
        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn build_with_auth_cli() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH", "cli");
        // CLI provider doesn't validate at build time → succeeds
        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn build_with_auth_default() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH", "default");
        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn build_with_auth_unknown_rejected() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH", "bogus");
        let result = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("unknown auth value"),
            "got: {err}"
        );
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let client = anthropic::Client::builder()
            .api_key(anthropic::client::AnthropicKey::from("sk-ant-test"))
            .build()
            .unwrap();

        let backend = AnthropicBackend {
            state: AnthropicRunState {
                client_state: AnthropicClientState::Static(client),
                model: "claude-sonnet-4-6".into(),
                tool_filter: None,
                client_params: HashMap::new(),
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
    fn auth_scope_default() {
        let _guard = isolated_env();
        let scope = crate::clients::config::azure_auth_scope(
            "ANTHROPIC_AUTH_SCOPE",
            "anthropic",
            "https://cognitiveservices.azure.com/.default",
        );
        assert_eq!(scope, "https://cognitiveservices.azure.com/.default");
    }

    #[test]
    fn auth_scope_custom() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH_SCOPE", "https://custom-scope.example.com");
        let scope = crate::clients::config::azure_auth_scope(
            "ANTHROPIC_AUTH_SCOPE",
            "anthropic",
            "https://cognitiveservices.azure.com/.default",
        );
        assert_eq!(scope, "https://custom-scope.example.com");
    }
}
