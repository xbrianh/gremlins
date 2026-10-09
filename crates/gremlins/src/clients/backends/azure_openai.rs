use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::providers::openai::{OpenAI, OpenAIConfig};
use rig_core::providers::openai::wire::AZURE;
use rig_core::driver::DynModel;
use rig_core::http_client::DynHttpClient;
use rig_core::operation::Completion;

use crate::clients::agent_loop::{
    default_classify, run_agent_loop, CancelToken, ErrorClassifier, LoopOpts, RunContext,
};
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::openai_protocol;
use crate::clients::protocol::CompletedRun;
use crate::clients::retry::{self, validate_max_retries, STREAM_IDLE_BACKOFF};
use crate::clients::token_provider::TokenProvider;
use crate::clients::config::ProviderAuth;
use rig_reqwest::ReqwestClient;

// ── AzureOpenAiClientState ───────────────────────────────────────────────

/// Either a statically-built client (for ApiKey / Token auth) or the
/// ingredients to build one dynamically per attempt (for identity-based auth).
enum AzureOpenAiClientState {
    Static(Box<OpenAI>),
    Dynamic {
        token_provider: Arc<dyn TokenProvider>,
        endpoint: String,
        api_version: String,
        http_client: DynHttpClient,
        auth_scope: String,
    },
}

// ── AzureOpenAiRunState ──────────────────────────────────────────────────

struct AzureOpenAiRunState {
    client_state: AzureOpenAiClientState,
    model: String,
    tool_filter: Option<Vec<String>>,
    client_params: HashMap<String, String>,
    last_ctx: Mutex<Option<RunContext>>,
    cancels: Mutex<HashMap<String, HashMap<u64, Arc<CancelToken>>>>,
    next_id: AtomicU64,
    log_label: String,
}

impl AzureOpenAiRunState {
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
            stream_events: params.stream_events.clone(),
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
            AzureOpenAiClientState::Static(client) => {
                let model = client.completion(&model_name).erase();
                run_agent_loop(
                    model,
                    prompt,
                    ctx,
                    cancel,
                    LoopOpts {
                        extra: self.extra_params(),
                        tool_filter: self.tool_filter.as_deref(),
                        classify_error: Some(default_classify as ErrorClassifier),
                        max_tokens: None,
                        skip_temperature: false,
                    },
                    interactive,
                )
                .await
            }
            AzureOpenAiClientState::Dynamic {
                token_provider,
                endpoint,
                api_version,
                http_client,
                auth_scope,
            } => {
                // Capture errors into a local so execution always flows
                // through the cancellation-map cleanup below.
                let dyn_result = async {
                    let scope = &auth_scope;
                    let token = token_provider
                        .get_token(scope)
                        .await
                        .map_err(|e| ClientError::Runtime {
                            message: format!("Azure OpenAI token acquisition failed: {e}"),
                        })?;
                    let client = OpenAIConfig::with_alternate_key(&AZURE, token)
                        .with_api_version(api_version)
                        .with_base_url(endpoint)
                        .connect(http_client.clone());
                    let model = client.completion(&model_name).erase();
                    run_agent_loop(
                        model,
                        prompt,
                        ctx,
                        cancel,
                        LoopOpts {
                            extra: self.extra_params(),
                            tool_filter: self.tool_filter.as_deref(),
                            classify_error: Some(default_classify as ErrorClassifier),
                            max_tokens: None,
                            skip_temperature: false,
                        },
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

// ── AzureOpenAiBackend ───────────────────────────────────────────────────

pub struct AzureOpenAiBackend {
    state: AzureOpenAiRunState,
}

/// Resolve the auth method for Azure OpenAI.
///
/// Precedence:
/// 1. `GREMLINS_AZURE_OPENAI_AUTH` env var
/// 2. `settings.yaml` → `azure-openai.auth`
/// 3. Fallback: token (env → settings) → api_key (env → settings)
fn resolve_auth() -> Result<ProviderAuth, String> {
    // 1. env var
    if let Ok(env_val) = std::env::var("GREMLINS_AZURE_OPENAI_AUTH") {
        let v = env_val.trim();
        if !v.is_empty() {
            return parse_provider_auth(v);
        }
    }

    // Load settings.yaml once, reuse across all credential checks.
    let cfg = load_config();
    let azure_cfg = cfg.as_ref().and_then(|c| c.azure_openai());

    // 2. settings.yaml azure-openai.auth
    if let Some(auth) = azure_cfg.and_then(|a| a.auth.as_ref()) {
        let v = auth.0.trim();
        if !v.is_empty() {
            return parse_provider_auth(v);
        }
    }

    // 3. Fall back to token → api-key
    // token: env then settings
    if let Ok(env_val) = std::env::var("GREMLINS_AZURE_OPENAI_TOKEN") {
        if !env_val.trim().is_empty() {
            return Ok(ProviderAuth::Token(env_val.trim().to_string()));
        }
    }
    if let Some(token) = azure_cfg.and_then(|a| a.token.as_ref()) {
        if !token.0.trim().is_empty() {
            return Ok(ProviderAuth::Token(token.0.trim().to_string()));
        }
    }

    // api_key: env then settings
    if let Ok(env_val) = std::env::var("GREMLINS_AZURE_OPENAI_API_KEY") {
        if !env_val.trim().is_empty() {
            return Ok(ProviderAuth::ApiKey(env_val.trim().to_string()));
        }
    }
    if let Some(key) = azure_cfg.and_then(|a| a.api_key.as_ref()) {
        if !key.0.trim().is_empty() {
            return Ok(ProviderAuth::ApiKey(key.0.trim().to_string()));
        }
    }

    Err("no credentials for provider \"azure-openai\": set GREMLINS_AZURE_OPENAI_AUTH, \
         or add azure-openai.auth / azure-openai.token / azure-openai.api-key in settings.yaml".to_string())
}

fn parse_provider_auth(v: &str) -> Result<ProviderAuth, String> {
    match v {
        "client-secret" => Ok(ProviderAuth::ClientSecret),
        "cli" => Ok(ProviderAuth::Cli),
        "managed-identity" => Ok(ProviderAuth::ManagedIdentity),
        "default" => Ok(ProviderAuth::DefaultAzure),
        other => Err(format!(
            "unknown auth value {other:?}: expected \"client-secret\", \"cli\", \"managed-identity\", or \"default\"",
        )),
    }
}

/// Resolve a string value with precedence: env var → settings.yaml → default.
fn resolve_string(
    env_var_name: &str,
    settings_getter: impl Fn() -> Option<String>,
    default: Option<&str>,
) -> Option<String> {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return Some(val);
        }
    }
    if let Some(val) = settings_getter() {
        if !val.trim().is_empty() {
            return Some(val);
        }
    }
    default.map(|s| s.to_string())
}

/// Parse configuration from disk, swallowing errors (returns None on failure).
fn load_config() -> Option<crate::config::Config> {
    crate::config::Config::load().ok()
}

impl AzureOpenAiBackend {
    /// Build an Azure OpenAI backend.
    ///
    /// Auth is resolved inline:
    ///
    /// | `azure-openai.auth` / `GREMLINS_AZURE_OPENAI_AUTH` | Behaviour |
    /// |---|---|
    /// | (unset) | Static fallback: `azure-openai.token` → `azure-openai.api-key` |
    /// | `"client-secret"` | Service principal via `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` / `AZURE_TENANT_ID` |
    /// | `"cli"` | `az account get-access-token` |
    /// | `"managed-identity"` | Azure IMDS endpoint |
    /// | `"default"` | Chains client-secret → CLI → managed identity |
    ///
    /// For dynamic methods the client is built per attempt (token acquisition
    /// is async).  Configuration errors (bad env vars) surface at first use.
    ///
    /// `GREMLINS_AZURE_OPENAI_ENDPOINT` (or `settings.yaml` `azure-openai.endpoint`) is required.
    /// `GREMLINS_AZURE_OPENAI_API_VERSION` (or `settings.yaml` `azure-openai.api-version`) defaults to `"2024-10-21"`.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let endpoint = resolve_string(
            "GREMLINS_AZURE_OPENAI_ENDPOINT",
            || {
                load_config()
                    .and_then(|c| c.azure_openai().and_then(|a| a.endpoint.as_ref().map(|s| s.0.clone())))
            },
            None,
        )
        .ok_or_else(|| {
            "GREMLINS_AZURE_OPENAI_ENDPOINT (or settings.yaml azure-openai.endpoint) is required for the Azure OpenAI backend".to_string()
        })?;

        let api_version = resolve_string(
            "GREMLINS_AZURE_OPENAI_API_VERSION",
            || {
                load_config()
                    .and_then(|c| c.azure_openai().and_then(|a| a.api_version.as_ref().map(|s| s.0.clone())))
            },
            Some("2024-10-21"),
        )
        .unwrap_or_else(|| "2024-10-21".to_string());

        let auth_method = resolve_auth()?;

        // Resolve auth_scope once for dynamic auth — avoids re-reading
        // settings.yaml on every token acquisition attempt.
        let auth_scope = resolve_string(
            "GREMLINS_AZURE_OPENAI_AUTH_SCOPE",
            || {
                load_config()
                    .and_then(|c| c.azure_openai().and_then(|a| a.auth_scope.as_ref().map(|s| s.0.clone())))
            },
            Some("https://cognitiveservices.azure.com/.default"),
        )
        .unwrap_or_else(|| "https://cognitiveservices.azure.com/.default".to_string());

        let model = if model.is_empty() {
            return Err("azure-openai backend requires a deployment name (e.g. azure-openai:gpt-4o)".into());
        } else {
            model.to_string()
        };

        let tool_filter = openai_protocol::tool_filter(native_block);
        let client_params = openai_protocol::string_map(extra_params);

        let http_client = DynHttpClient::new(ReqwestClient::default());

        let client_state = match auth_method {
            ProviderAuth::ApiKey(key) => {
                let client = OpenAIConfig::with_key(&AZURE, key)
                    .with_api_version(&api_version)
                    .with_base_url(&endpoint)
                    .connect(http_client.clone());
                AzureOpenAiClientState::Static(Box::new(client))
            }
            ProviderAuth::Token(token) => {
                let client = OpenAIConfig::with_alternate_key(&AZURE, token)
                    .with_api_version(&api_version)
                    .with_base_url(&endpoint)
                    .connect(http_client.clone());
                AzureOpenAiClientState::Static(Box::new(client))
            }
            ProviderAuth::ClientSecret => AzureOpenAiClientState::Dynamic {
                token_provider: Arc::new(
                    crate::clients::token_provider::ClientSecretProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::Cli => AzureOpenAiClientState::Dynamic {
                token_provider: Arc::new(
                    crate::clients::token_provider::AzureCliProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::ManagedIdentity => AzureOpenAiClientState::Dynamic {
                token_provider: Arc::new(
                    crate::clients::token_provider::ManagedIdentityProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::DefaultAzure => AzureOpenAiClientState::Dynamic {
                token_provider: Arc::new(
                    crate::clients::token_provider::DefaultAzureProvider::new(),
                ),
                endpoint,
                api_version,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
        };

        Ok(Arc::new(Self {
            state: AzureOpenAiRunState {
                client_state,
                model,
                tool_filter,
                client_params,
                last_ctx: Mutex::new(None),
                cancels: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log_label: "AzureOpenAiBackend".to_string(),
            },
        }))
    }
}

#[async_trait]
impl Backend for AzureOpenAiBackend {
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

    fn make_model(&self, spec: &str) -> Option<DynModel<Completion>> {
        let (provider, model) = openai_protocol::provider_and_model(spec)?;
        if provider == "azure-openai" {
            match &self.state.client_state {
                AzureOpenAiClientState::Static(client) => {
                    Some(client.completion(model).erase())
                }
                AzureOpenAiClientState::Dynamic {
                    ref endpoint,
                    ref api_version,
                    ref http_client,
                    ref token_provider,
                    ref auth_scope,
                } => {
                    // Wrap the HTTP client so the api-key header is
                    // populated lazily on the first request — make_model
                    // is synchronous and cannot block on token acquisition.
                    let wrapped = crate::clients::lazy_auth_http::LazyApiKeyHttpClient::new(
                        http_client.clone(),
                        token_provider.clone(),
                        auth_scope.clone(),
                    );
                    let client = OpenAIConfig::with_alternate_key(&AZURE, "unused")
                        .with_api_version(api_version)
                        .with_base_url(endpoint)
                        .connect(wrapped);
                    Some(client.completion(model).erase())
                }
            }
        } else {
            None
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
        guard.remove("GREMLINS_AZURE_OPENAI_TOKEN");
        guard.remove("GREMLINS_AZURE_OPENAI_API_KEY");
        guard.remove("GREMLINS_AZURE_OPENAI_ENDPOINT");
        guard.remove("GREMLINS_AZURE_OPENAI_API_VERSION");
        guard.remove("GREMLINS_AZURE_OPENAI_AUTH");
        guard.remove("GREMLINS_AZURE_OPENAI_AUTH_SCOPE");
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

        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("GREMLINS_AZURE_OPENAI_ENDPOINT"),
            "got: {err}"
        );
    }

    #[test]
    fn build_rejects_empty_model() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "fake-key");

        let result = AzureOpenAiBackend::build(
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
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");

        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        let err = result.err().expect("should be an error");
        assert!(
            err.contains("no credentials for provider \"azure-openai\""),
            "got: {err}"
        );
    }

    // ── auth precedence tests ─────────────────────────────────────────

    #[test]
    fn auth_precedence_token_over_api_key() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_TOKEN", "bearer-token-123");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "api-key-456");

        let method = resolve_auth().unwrap();
        assert!(
            matches!(method, ProviderAuth::Token(t) if t == "bearer-token-123"),
            "GREMLINS_AZURE_OPENAI_TOKEN should win over GREMLINS_AZURE_OPENAI_API_KEY"
        );
    }

    #[test]
    fn auth_precedence_env_over_settings_yaml() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "env-api-key");

        // Write a settings.yaml — env var should win over it.
        let sandbox_root = std::env::var("GREMLINS_SANDBOX_ROOT").unwrap();
        let config_dir = std::path::PathBuf::from(&sandbox_root).join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.yaml"),
            r#"{"azure-openai": {"api-key": "settings-yaml-key"}}"#,
        )
        .unwrap();
        crate::config::init_global().unwrap();

        let method = resolve_auth().unwrap();
        assert!(
            matches!(method, ProviderAuth::ApiKey(k) if k == "env-api-key"),
            "GREMLINS_AZURE_OPENAI_API_KEY env var should win over settings.yaml"
        );
    }

    #[test]
    fn api_version_defaults() {
        let mut guard = isolated_env();
        guard.remove("GREMLINS_AZURE_OPENAI_API_VERSION");
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "fake-key");

        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok(), "build should succeed with default api version");
    }

    #[test]
    fn api_version_from_env() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_API_VERSION", "2025-01-01");
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "fake-key");

        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok(), "build should succeed with custom api version");
    }

    #[test]
    fn extra_params_passthrough() {
        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "1024".into());
        extra.insert("temperature".into(), "0.7".into());

        let client_params = openai_protocol::string_map(&extra);
        let state = AzureOpenAiRunState {
            client_state: AzureOpenAiClientState::Static(Box::new(
                OpenAIConfig::with_key(&AZURE, "fake-key")
            .with_api_version("2024-10-21")
            .with_base_url("https://example.openai.azure.com")
            .client(),
            )),
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
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "fake-key");

        let client = OpenAIConfig::with_key(&AZURE, "fake-key")
            .with_api_version("2024-10-21")
            .with_base_url("https://example.openai.azure.com")
            .client();

        let backend = AzureOpenAiBackend {
            state: AzureOpenAiRunState {
                client_state: AzureOpenAiClientState::Static(Box::new(client)),
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
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "client-secret");
        // ClientSecretProvider::new() defers env-var validation to first
        // get_token() call, so build() succeeds even without env vars.
        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_cli_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "cli");
        // CLI provider doesn't validate at build time → succeeds
        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_managed_identity_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "managed-identity");
        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_default_parses() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "default");
        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_unknown_rejected() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "bogus");
        let result = AzureOpenAiBackend::build(
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
    fn auth_method_env_over_settings_yaml() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "cli");

        let sandbox_root = std::env::var("GREMLINS_SANDBOX_ROOT").unwrap();
        let config_dir = std::path::PathBuf::from(&sandbox_root).join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.yaml"),
            r#"{"azure-openai": {"auth": "managed-identity"}}"#,
        )
        .unwrap();
        crate::config::init_global().unwrap();

        let method = resolve_auth().unwrap();
        assert!(
            matches!(method, ProviderAuth::Cli),
            "GREMLINS_AZURE_OPENAI_AUTH env var should win over settings.yaml"
        );
    }

    #[test]
    fn auth_method_absent_with_api_key_falls_back() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "my-key");
        // No auth field set → falls back to api-key
        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn auth_method_absent_with_token_falls_back() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_TOKEN", "my-token");
        let result = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        );
        assert!(result.is_ok());
    }

    // ── make_model tests ────────────────────────────────────────────

    /// `make_model` for a Static (API-key) client returns Some.
    #[test]
    fn make_model_static_client() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "fake-key");
        let backend = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        let model = backend.make_model("azure-openai:gpt-4o");
        assert!(model.is_some(), "Static client must produce a model");
    }

    /// `make_model` for a Dynamic (CLI auth) client returns Some.
    /// The lazy wrapper holds the same TokenProvider as single_attempt.
    #[test]
    fn make_model_dynamic_client() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_AUTH", "cli");
        let backend = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        let model = backend.make_model("azure-openai:gpt-4o");
        assert!(
            model.is_some(),
            "Dynamic client must produce a model (lazy token acquisition)"
        );
    }

    /// `make_model` returns None for non-azure provider specs.
    #[test]
    fn make_model_rejects_other_providers() {
        let mut guard = isolated_env();
        guard.set("GREMLINS_AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com");
        guard.set("GREMLINS_AZURE_OPENAI_API_KEY", "fake-key");
        let backend = AzureOpenAiBackend::build(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        assert!(backend.make_model("openai:gpt-4o").is_none());
        assert!(backend.make_model("anthropic:claude-sonnet-4-6").is_none());
    }
}