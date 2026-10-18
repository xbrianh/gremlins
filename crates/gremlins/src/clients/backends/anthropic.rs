use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rig_core::http_client::DynHttpClient;
use rig_core::providers::anthropic::{Anthropic, AnthropicConfig};

use crate::clients::agent_loop::CancelToken;
use crate::clients::anthropic_bearer_http::BearerHttpClient;
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::cancels::CancelsMap;
use crate::clients::config::ProviderAuth;
use crate::clients::interactive::InteractiveSession;
use crate::clients::lazy_auth_http::{BearerStrategy, LazyAuthHttpClient};
use crate::clients::model::GremlinsModel;
use crate::clients::native::NativeRunState;
use crate::clients::openai_protocol;
use crate::clients::params::{self, ResolvedParams};
use crate::clients::protocol::CompletedRun;
use crate::clients::token_provider::{self, TokenProvider};
use rig_reqwest::ReqwestClient;

/// Default `max_tokens` for Anthropic requests.
/// Current-generation models (Opus 5.5, Sonnet 5.5, Fable 5.1) support up to 128_000 output tokens.
const DEFAULT_MAX_TOKENS: u64 = 128_000;

// ── AnthropicClientState ─────────────────────────────────────────────────

/// Either a statically-built client (for ApiKey / Token auth) or the
/// ingredients to build one dynamically per attempt (for identity-based auth).
enum AnthropicClientState {
    Static(Anthropic),
    Dynamic {
        token_provider: Arc<dyn TokenProvider>,
        base_url: String,
        http_client: DynHttpClient,
        auth_scope: String,
    },
}

// ── AnthropicBackend ─────────────────────────────────────────────────────

pub struct AnthropicBackend {
    state: NativeRunState<AnthropicClientState>,
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
        Self::build_inner(model, native_block, extra_params)
            .map(|b| Arc::new(b) as Arc<dyn Backend>)
    }

    /// Shared construction — returns the concrete type so both `build`
    /// and test helpers can call it.
    fn build_inner(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Self, String> {
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
        let resolved = Self::resolve_params(extra_params)?;

        let http_client = DynHttpClient::new(ReqwestClient::default());

        // Resolve auth_scope once for dynamic auth — avoids re-reading
        // settings.yaml on every token acquisition attempt.
        let auth_scope = crate::clients::config::azure_auth_scope(
            "ANTHROPIC_AUTH_SCOPE",
            "anthropic",
            "https://cognitiveservices.azure.com/.default",
        );

        let client_state = match auth_method {
            ProviderAuth::ApiKey(key) => {
                let client = AnthropicConfig::new(key)
                    .with_base_url(&base_url)
                    .connect(http_client.clone());
                AnthropicClientState::Static(client)
            }
            ProviderAuth::Token(token) => {
                let client = AnthropicConfig::new(token)
                    .with_base_url(&base_url)
                    .connect(http_client.clone());
                AnthropicClientState::Static(client)
            }
            ProviderAuth::ClientSecret => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::ClientSecretProvider::new()),
                base_url,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::Cli => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::AzureCliProvider::new()),
                base_url,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::ManagedIdentity => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::ManagedIdentityProvider::new()),
                base_url,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::DefaultAzure => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::DefaultAzureProvider::new()),
                base_url,
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
        };

        Ok(Self {
            state: NativeRunState {
                client_state,
                model,
                tool_filter,
                resolved,
                last_ctx: Mutex::new(None),
                cancels: CancelsMap::default(),
                next_id: AtomicU64::new(1),
                log_label: "AnthropicBackend".to_string(),
            },
        })
    }
}

#[async_trait]
impl Backend for AnthropicBackend {
    fn make_model(&self, spec: &str) -> Option<GremlinsModel> {
        let (provider, model) = openai_protocol::provider_and_model(spec)?;
        if provider == "anthropic" {
            match &self.state.client_state {
                AnthropicClientState::Static(client) => {
                    Some(GremlinsModel::new(
                        client.completion(&model).erase(),
                        crate::config::max_prompt_bytes(),
                    ))
                }
                AnthropicClientState::Dynamic {
                    ref base_url,
                    ref http_client,
                    ref token_provider,
                    ref auth_scope,
                } => {
                    // Wrap the HTTP client so the bearer token is acquired
                    // lazily on the first request — make_model is synchronous
                    // and cannot block on token acquisition.
                    let wrapped = LazyAuthHttpClient::<BearerStrategy>::new(
                        http_client.clone(),
                        token_provider.clone(),
                        auth_scope.clone(),
                    );
                    let client = AnthropicConfig::new("unused")
                        .with_base_url(base_url)
                        .connect(wrapped);
                    Some(GremlinsModel::new(
                        client.completion(&model).erase(),
                        crate::config::max_prompt_bytes(),
                    ))
                }
            }
        } else {
            None
        }
    }

    async fn run(
        &self,
        params: RunParams,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        let ctx = self.state.prepare_context(params.clone());
        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);
        let model_name = self.state.effective_model(ctx.params.model.as_deref());

        let make_model = async {
            match &self.state.client_state {
                AnthropicClientState::Static(client) => {
                    Ok(GremlinsModel::new(
                        client.completion(&model_name).erase(),
                        crate::config::max_prompt_bytes(),
                    ))
                }
                AnthropicClientState::Dynamic {
                    token_provider,
                    base_url,
                    http_client,
                    auth_scope,
                } => {
                    let token =
                        token_provider.get_token(auth_scope.as_str()).await.map_err(
                            |e| ClientError::Runtime {
                                message: format!("Anthropic token acquisition failed: {e}"),
                            },
                        )?;
                    let wrapped =
                        BearerHttpClient::new(http_client.clone(), token).map_err(|e| {
                            ClientError::Runtime {
                                message: format!("failed to build bearer HTTP client: {e}"),
                            }
                        })?;
                    let client = AnthropicConfig::new("unused")
                        .with_base_url(base_url)
                        .connect(wrapped);
                    Ok(GremlinsModel::new(
                        client.completion(&model_name).erase(),
                        crate::config::max_prompt_bytes(),
                    ))
                }
            }
        };

        self.state
            .single_attempt(&params.prompt, ctx, cancel, interactive, make_model)
            .await
    }

    fn last_run_params(&self) -> Result<RunParams, ClientError> {
        self.state.last_run_params()
    }

    fn reap_all(&self, gremlin_id: &str) {
        self.state.reap(gremlin_id)
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }

    fn resolved_params(&self) -> Option<ResolvedParams> {
        Some(self.state.resolved.clone())
    }

    fn resolve_params(raw: &indexmap::IndexMap<String, String>) -> Result<ResolvedParams, String> {
        let mut additional = serde_json::Map::new();
        // Enable prompt caching by default (5-min TTL). Users can override via
        // cache_control=... or disable with cache_control=null.
        additional.insert(
            "cache_control".to_string(),
            serde_json::json!({ "type": "ephemeral" }),
        );

        let mut temperature = None;
        let mut max_tokens = Some(DEFAULT_MAX_TOKENS);

        for (k, v) in raw {
            match k.as_str() {
                "temperature" => temperature = Some(params::parse_f64("anthropic", k, v)?),
                "max_tokens" => max_tokens = Some(params::parse_u64("anthropic", k, v)?),
                "cache_control" => {
                    // `null` disables prompt caching; anything else must be a
                    // JSON object.
                    if v == "null" {
                        additional.insert(k.clone(), serde_json::Value::Null);
                    } else {
                        let obj = params::parse_json_object("anthropic", k, v)?;
                        additional.insert(k.clone(), obj);
                    }
                }
                _ => {
                    additional.insert(k.clone(), params::coerce(v));
                }
            }
        }

        Ok(ResolvedParams {
            temperature,
            max_tokens,
            additional,
        })
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) enum ClientStateVariant {
    Static,
    Dynamic,
}

#[cfg(test)]
impl AnthropicBackend {
    pub(crate) fn client_state_variant(&self) -> ClientStateVariant {
        match &self.state.client_state {
            AnthropicClientState::Static(_) => ClientStateVariant::Static,
            AnthropicClientState::Dynamic { .. } => ClientStateVariant::Dynamic,
        }
    }

    /// Test-only build that returns the concrete type so tests can inspect
    /// internal state before it is erased to `Arc<dyn Backend>`.
    pub(crate) fn build_concrete(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Self, String> {
        Self::build_inner(model, native_block, extra_params)
    }
}

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
    fn identity_auth_produces_dynamic_state() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH", "cli");
        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        // Verify the concrete variant before it's erased to Arc<dyn Backend>.
        assert!(
            matches!(backend.client_state_variant(), ClientStateVariant::Dynamic),
            "CLI auth must produce Dynamic state"
        );
    }

    #[test]
    fn api_key_auth_produces_static_state() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");
        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        assert!(
            matches!(backend.client_state_variant(), ClientStateVariant::Static),
            "API key auth must produce Static state"
        );
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

        let client = Anthropic::new("sk-ant-test");

        let backlog = CancelsMap::default();
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        backlog.insert("gr-test", 1, a.clone()).unwrap();
        backlog.insert("gr-test", 2, b.clone()).unwrap();
        backlog.insert("gr-sibling", 3, sibling.clone()).unwrap();

        let backend = AnthropicBackend {
            state: NativeRunState {
                client_state: AnthropicClientState::Static(client),
                model: "claude-sonnet-4-6".into(),
                tool_filter: None,
                resolved: ResolvedParams::default(),
                last_ctx: Mutex::new(None),
                cancels: backlog,
                next_id: AtomicU64::new(1),
                log_label: "test".into(),
            },
        };
        backend.reap_all("gr-test");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(!sibling.is_cancelled());
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

    // ── max_tokens resolution tests ──────────────────────────────────

    /// When no `max_tokens` appears in client params, the resolved value
    /// is the default 128_000.
    #[test]
    fn build_max_tokens_default_128000() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        assert_eq!(
            backend.state.resolved.max_tokens,
            Some(128_000),
            "default max_tokens must be 128_000"
        );
    }

    /// `max_tokens=8192` in client params overrides the default.
    #[test]
    fn build_max_tokens_override_8192() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "8192".into());

        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &extra,
        )
        .unwrap();
        assert_eq!(backend.state.resolved.max_tokens, Some(8192));
    }

    /// A malformed `max_tokens` value (not a valid u64) is a build error.
    #[test]
    fn build_max_tokens_malformed_is_error() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "not-a-number".into());

        let err = AnthropicBackend::build(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &extra,
        )
        .err()
        .expect("malformed max_tokens must error");
        assert!(err.contains("max_tokens"), "{err}");
    }

    /// `max_tokens` is stripped from `additional_params` so it is not
    /// sent twice (once as the top-level field and once in the extras blob).
    #[test]
    fn extra_params_excludes_max_tokens() {
        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "4096".into());
        extra.insert("thinking".into(), "{\"type\":\"enabled\"}".into());

        let resolved = AnthropicBackend::resolve_params(&extra).unwrap();
        let obj = resolved.additional_value().unwrap();
        assert!(
            obj.get("max_tokens").is_none(),
            "max_tokens must not appear in extra params"
        );
        assert!(
            obj.get("thinking").is_some(),
            "other params must still be forwarded"
        );
    }

    // ── cache_control default / override tests ───────────────────────

    /// Absent `cache_control` key → default `{"type":"ephemeral"}` is inserted.
    #[test]
    fn cache_control_default_ephemeral_when_absent() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();

        let val = backend.state.resolved.additional.get("cache_control");
        assert!(
            val.is_some(),
            "cache_control must be present when not explicitly set"
        );
        assert_eq!(
            val.unwrap(),
            &serde_json::json!({"type": "ephemeral"}),
            "default cache_control must be ephemeral"
        );
    }

    /// Explicit `cache_control=null` is preserved (not overwritten by the default).
    #[test]
    fn cache_control_null_preserved() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let mut extra = indexmap::IndexMap::new();
        extra.insert("cache_control".into(), "null".into());

        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &extra,
        )
        .unwrap();

        let val = backend.state.resolved.additional.get("cache_control");
        assert!(
            val.is_some(),
            "cache_control key must still be present when set to null"
        );
        assert_eq!(
            val.unwrap(),
            &serde_json::Value::Null,
            "explicit cache_control=null must not be overwritten"
        );
    }

    // ── make_model tests ────────────────────────────────────────────

    /// `make_model` for a Static (API-key) client returns Some.
    #[test]
    fn make_model_static_client() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");
        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        let model = backend.make_model("anthropic/claude-sonnet-4-6");
        assert!(model.is_some(), "Static client must produce a model");
    }

    /// `make_model` for a Dynamic (CLI auth) client returns Some.
    /// The lazy wrapper holds the same TokenProvider as single_attempt.
    #[test]
    fn make_model_dynamic_client() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_AUTH", "cli");
        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        let model = backend.make_model("anthropic/claude-sonnet-4-6");
        assert!(
            model.is_some(),
            "Dynamic client must produce a model (lazy token acquisition)"
        );
    }

    /// `make_model` returns None for non-anthropic provider specs.
    #[test]
    fn make_model_rejects_other_providers() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");
        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
        )
        .unwrap();
        assert!(backend.make_model("openai/gpt-4o").is_none());
        assert!(backend.make_model("azure-openai/gpt-4o").is_none());
    }

    /// Explicit `cache_control` with a custom TTL wins over the default.
    #[test]
    fn cache_control_custom_ttl_wins() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let mut extra = indexmap::IndexMap::new();
        extra.insert(
            "cache_control".into(),
            "{\"type\":\"ephemeral\",\"ttl\":\"1h\"}".into(),
        );

        let backend = AnthropicBackend::build_concrete(
            "claude-sonnet-4-6",
            &HashMap::new(),
            &extra,
        )
        .unwrap();

        let val = backend.state.resolved.additional.get("cache_control");
        assert!(
            val.is_some(),
            "cache_control must be present when explicitly set"
        );
        assert_eq!(
            val.unwrap(),
            &serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "user-supplied cache_control must not be overwritten"
        );
    }
}
