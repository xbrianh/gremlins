use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::client::CompletionClient;
use rig_core::providers::azure::{self, AzureOpenAIAuth};

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::openai_protocol;
use crate::clients::protocol::CompletedRun;
use crate::clients::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use crate::clients::token_provider::TokenProvider;
use crate::config::ProviderAuth;
use rig_core::http_client::ReqwestClient;

// ── AzureClientState ─────────────────────────────────────────────────────

/// Either a statically-built client (for ApiKey / Token auth) or the
/// ingredients to build one dynamically per attempt (for identity-based auth).
enum AzureClientState {
    Static(azure::Client),
    Dynamic {
        token_provider: Box<dyn TokenProvider>,
        endpoint: String,
        api_version: String,
        http_client: ReqwestClient,
    },
}

// ── AzureRunState ────────────────────────────────────────────────────────

struct AzureRunState {
    client_state: AzureClientState,
    model: String,
    tool_filter: Option<Vec<String>>,
    client_params: HashMap<String, String>,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    next_id: AtomicU64,
    log_label: String,
}

impl AzureRunState {
    fn extra_params(&self) -> Option<serde_json::Value> {
        openai_protocol::build_extra_params(&self.client_params)
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
            AzureClientState::Static(client) => {
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
            AzureClientState::Dynamic {
                token_provider,
                endpoint,
                api_version,
                http_client,
            } => {
                // Capture errors into a local so execution always flows
                // through the cancellation-map cleanup below.
                let dyn_result = async {
                    let scope = crate::config::auth_scope("GREMLINS_AZURE_AUTH_SCOPE", "azure-foundry", "https://cognitiveservices.azure.com/.default");
                    let token = token_provider
                        .get_token(&scope)
                        .await
                        .map_err(|e| ClientError::Runtime {
                            message: format!("Azure token acquisition failed: {e}"),
                        })?;
                    let client = azure::Client::builder()
                        .api_key(AzureOpenAIAuth::Token(token))
                        .api_version(api_version)
                        .azure_endpoint(endpoint.clone())
                        .http_client(http_client.clone())
                        .build()
                        .map_err(|e| ClientError::Runtime {
                            message: format!("failed to build Azure client: {e}"),
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

// ── AzureBackend ─────────────────────────────────────────────────────────

pub struct AzureBackend {
    state: AzureRunState,
}

impl AzureBackend {
    /// Build an Azure backend.
    ///
    /// Auth is resolved via [`crate::config::resolve_azure_auth_method`]:
    ///
    /// | `azure.auth` / `GREMLINS_AZURE_AUTH` | Behaviour |
    /// |---|---|
    /// | (unset) | Static fallback: `azure.token` → `azure.api-key` |
    /// | `"client-secret"` | Service principal via `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` / `AZURE_TENANT_ID` |
    /// | `"cli"` | `az account get-access-token` |
    /// | `"managed-identity"` | Azure IMDS endpoint |
    /// | `"default"` | Chains client-secret → CLI → managed identity |
    ///
    /// For dynamic methods the client is built per attempt (token acquisition
    /// is async).  Configuration errors (bad env vars) surface at first use.
    ///
    /// `GREMLINS_AZURE_ENDPOINT` (or settings.yaml `azure.endpoint`) is required.
    /// `GREMLINS_AZURE_API_VERSION` (or settings.yaml `azure.api-version`) defaults to `"2024-10-21"`.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let endpoint = crate::config::endpoint("GREMLINS_AZURE_ENDPOINT", "azure-foundry").ok_or_else(|| {
            "GREMLINS_AZURE_ENDPOINT (or providers.yaml azure-foundry.endpoint) is required for the Azure backend".to_string()
        })?;

        let api_version = crate::config::api_version("GREMLINS_AZURE_API_VERSION", "azure-foundry", "2024-10-21");

        let auth_method = crate::config::auth_method("GREMLINS_AZURE_AUTH", "azure-foundry", "GREMLINS_AZURE_TOKEN", "GREMLINS_AZURE_API_KEY")?;

        let model = if model.is_empty() {
            return Err("azure backend requires a deployment name (e.g. azure:gpt-4o)".into());
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
                let client = azure::Client::builder()
                    .api_key(AzureOpenAIAuth::ApiKey(key))
                    .api_version(&api_version)
                    .azure_endpoint(endpoint)
                    .build()
                    .map_err(|e| format!("failed to build Azure client: {e}"))?;
                AzureClientState::Static(client)
            }
            ProviderAuth::Token(token) => {
                let client = azure::Client::builder()
                    .api_key(AzureOpenAIAuth::Token(token))
                    .api_version(&api_version)
                    .azure_endpoint(endpoint)
                    .build()
                    .map_err(|e| format!("failed to build Azure client: {e}"))?;
                AzureClientState::Static(client)
            }
            ProviderAuth::ClientSecret => AzureClientState::Dynamic {
                token_provider: Box::new(
                    crate::clients::token_provider::ClientSecretProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
            },
            ProviderAuth::Cli => AzureClientState::Dynamic {
                token_provider: Box::new(
                    crate::clients::token_provider::AzureCliProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
            },
            ProviderAuth::ManagedIdentity => AzureClientState::Dynamic {
                token_provider: Box::new(
                    crate::clients::token_provider::ManagedIdentityProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
            },
            ProviderAuth::DefaultAzure => AzureClientState::Dynamic {
                token_provider: Box::new(
                    crate::clients::token_provider::DefaultAzureProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
            },
        };

        Ok(Arc::new(Self {
            state: AzureRunState {
                client_state,
                model,
                tool_filter,
                client_params,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "AzureBackend".to_string(),
            },
        }))
    }
}

#[async_trait]
impl Backend for AzureBackend {
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

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::super::agent_loop::CancelToken;
    use super::*;
    use crate::test_support::EnvGuard;

    fn scrub_azure_env(guard: &mut EnvGuard) {
        guard.remove("GREMLINS_AZURE_TOKEN");
        guard.remove("GREMLINS_AZURE_API_KEY");
        guard.remove("GREMLINS_AZURE_ENDPOINT");
        guard.remove("GREMLINS_AZURE_API_VERSION");
        guard.remove("GREMLINS_AZURE_AUTH");
        guard.remove("GREMLINS_AZURE_AUTH_SCOPE");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        guard.remove("AZURE_TENANT_ID");
    }

    fn isolated_env() -> EnvGuard {
        let mut guard = EnvGuard::lock();
        scrub_azure_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap().keep();
        guard.set("GREMLINS_SANDBOX_ROOT", &tmp);
        guard.set("HOME", &tmp);
        guard
    }

    #[test]
    fn build_rejects_missing_endpoint() {
        let _guard = isolated_env();

        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("GREMLINS_AZURE_ENDPOINT"),
            "got: {err}"
        );
    }

    #[test]
    fn build_rejects_empty_model() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_API_KEY", "fake-key");

        let result = AzureBackend::build(
            "",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("deployment name"),
            "got: {err}"
        );
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");

        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("no credentials for provider \"azure-foundry\""),
            "got: {err}"
        );
    }

    // ── auth precedence tests ─────────────────────────────────────────

    #[test]
    fn auth_precedence_token_over_api_key() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_TOKEN", "bearer-token-123");
        guard.set("GREMLINS_AZURE_API_KEY", "api-key-456");

        let method = crate::config::auth_method("GREMLINS_AZURE_AUTH", "azure-foundry", "GREMLINS_AZURE_TOKEN", "GREMLINS_AZURE_API_KEY").unwrap();
        assert!(
            matches!(method, ProviderAuth::Token(t) if t == "bearer-token-123"),
            "GREMLINS_AZURE_TOKEN should win over GREMLINS_AZURE_API_KEY"
        );
    }

    #[test]
    fn auth_precedence_env_over_providers_yaml() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_API_KEY", "env-api-key");

        // Write a providers.yaml — env var should win over it.
        let sandbox_root = std::env::var("GREMLINS_SANDBOX_ROOT").unwrap();
        let config_dir = std::path::PathBuf::from(&sandbox_root).join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"azure-foundry": {"api-key": "providers-yaml-key"}}"#,
        )
        .unwrap();
        crate::config::init_global().unwrap();

        let method = crate::config::auth_method("GREMLINS_AZURE_AUTH", "azure-foundry", "GREMLINS_AZURE_TOKEN", "GREMLINS_AZURE_API_KEY").unwrap();
        assert!(
            matches!(method, ProviderAuth::ApiKey(k) if k == "env-api-key"),
            "GREMLINS_AZURE_API_KEY env var should win over providers.yaml"
        );
    }

    #[test]
    fn api_version_defaults() {
        let mut guard = isolated_env();
        guard.remove("GREMLINS_AZURE_API_VERSION");
        assert_eq!(crate::config::api_version("GREMLINS_AZURE_API_VERSION", "azure-foundry", "2024-10-21"), "2024-10-21");
    }

    #[test]
    fn api_version_from_env() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_API_VERSION", "2025-01-01");
        assert_eq!(crate::config::api_version("GREMLINS_AZURE_API_VERSION", "azure-foundry", "2024-10-21"), "2025-01-01");
    }

    #[test]
    fn extra_params_passthrough() {
        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "1024".into());
        extra.insert("temperature".into(), "0.7".into());

        let client_params = openai_protocol::string_map(&extra);
        let state = AzureRunState {
            client_state: AzureClientState::Static(
                azure::Client::builder()
                    .api_key(AzureOpenAIAuth::ApiKey("fake-key".into()))
                    .api_version("2024-10-21")
                    .azure_endpoint("https://example.openai.azure.com".to_string())
                    .build()
                    .unwrap(),
            ),
            model: "gpt-4o".into(),
            tool_filter: None,
            client_params,
            last_ctx: Mutex::new(None),
            cancels: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            log_label: "test".into(),
        };

        let obj = state.extra_params().expect("extra_params should be Some");
        assert_eq!(obj["max_tokens"], 1024);
        assert_eq!(obj["temperature"], 0.7);
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_API_KEY", "fake-key");

        let client = azure::Client::builder()
            .api_key(AzureOpenAIAuth::ApiKey("fake-key".into()))
            .api_version("2024-10-21")
            .azure_endpoint("https://example.openai.azure.com".to_string())
            .build()
            .unwrap();

        let backend = AzureBackend {
            state: AzureRunState {
                client_state: AzureClientState::Static(client),
                model: "gpt-4o".into(),
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

    // ── auth method tests ─────────────────────────────────────────────

    #[test]
    fn auth_method_client_secret_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_AUTH", "client-secret");
        // ClientSecretProvider::new() defers env-var validation to first
        // get_token() call, so build() succeeds even without env vars.
        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_cli_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_AUTH", "cli");
        // CLI provider doesn't validate at build time → succeeds
        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_managed_identity_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_AUTH", "managed-identity");
        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_default_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_AUTH", "default");
        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_unknown_rejected() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_AUTH", "bogus");
        let result = AzureBackend::build(
            "gpt-4o",
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
    fn auth_method_settings_yaml_over_env() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_AUTH", "cli");

        let sandbox_root = std::env::var("GREMLINS_SANDBOX_ROOT").unwrap();
        let config_dir = std::path::PathBuf::from(&sandbox_root).join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"azure-foundry": {"auth": "managed-identity"}}"#,
        )
        .unwrap();
        crate::config::init_global().unwrap();

        let method = crate::config::auth_method("GREMLINS_AZURE_AUTH", "azure-foundry", "GREMLINS_AZURE_TOKEN", "GREMLINS_AZURE_API_KEY").unwrap();
        assert!(
            matches!(method, ProviderAuth::ManagedIdentity),
            "providers.yaml azure-foundry.auth should win over GREMLINS_AZURE_AUTH"
        );
    }

    #[test]
    fn auth_method_absent_with_api_key_falls_back() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_API_KEY", "my-key");
        // No auth field set → falls back to api-key
        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_absent_with_token_falls_back() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_TOKEN", "my-token");
        let result = AzureBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }
}