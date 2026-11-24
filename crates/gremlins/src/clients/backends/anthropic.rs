use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rig_core::http_client::DynHttpClient;
use rig_core::providers::anthropic::{Anthropic, AnthropicConfig};

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::cancel::CancelToken;
use crate::clients::config::ProviderAuth;
use crate::clients::interactive::InteractiveSession;
use crate::clients::lazy_auth_http::{BearerStrategy, LazyAuthHttpClient};

use crate::clients::native::NativeRunState;
use crate::clients::params::{self, ResolvedParams};
use crate::clients::protocol::{CompactionBlock, CompactionResult, CompletedRun};
use crate::clients::token_provider::{self, TokenProvider};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
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

        let auth_method = crate::clients::config::azure_auth_method(
            "ANTHROPIC_AUTH",
            "anthropic",
            "ANTHROPIC_TOKEN",
            "ANTHROPIC_API_KEY",
        )?;

        let model = if model.is_empty() {
            "claude-sonnet-4-6".to_string()
        } else {
            model.to_string()
        };

        let tool_filter = crate::clients::config::tool_filter(native_block);
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
                let client = AnthropicConfig::new(key.clone())
                    .with_beta("compact-2026-09-04")
                    .with_base_url(&base_url)
                    .connect(http_client.clone());
                AnthropicClientState::Static(client)
            }
            ProviderAuth::Token(token) => {
                let client = AnthropicConfig::new(token.clone())
                    .with_beta("compact-2026-09-04")
                    .with_base_url(&base_url)
                    .connect(http_client.clone());
                AnthropicClientState::Static(client)
            }
            ProviderAuth::ClientSecret => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::ClientSecretProvider::new()),
                base_url: base_url.clone(),
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::Cli => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::AzureCliProvider::new()),
                base_url: base_url.clone(),
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::ManagedIdentity => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::ManagedIdentityProvider::new()),
                base_url: base_url.clone(),
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
            ProviderAuth::DefaultAzure => AnthropicClientState::Dynamic {
                token_provider: Arc::new(token_provider::DefaultAzureProvider::new()),
                base_url: base_url.clone(),
                http_client: http_client.clone(),
                auth_scope: auth_scope.clone(),
            },
        };

        Ok(Self {
            state: NativeRunState::new(
                client_state,
                model,
                tool_filter,
                resolved,
                "AnthropicBackend",
            ),
        })
    }
}

#[async_trait]
impl Backend for AnthropicBackend {
    fn build_model(&self, spec: &str) -> Option<DynModel<Completion>> {
        let (provider, model) = crate::model_uri::provider_and_model(spec)?;
        if provider == "anthropic" {
            match &self.state.client_state {
                AnthropicClientState::Static(client) => Some(client.completion(&model).erase()),
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
                        .with_beta("compact-2026-09-04")
                        .with_base_url(base_url)
                        .connect(wrapped);
                    Some(client.completion(&model).erase())
                }
            }
        } else {
            None
        }
    }

    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        let ctx = self.state.prepare_context(params.clone());
        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);
        let model_name = self.state.effective_model(ctx.params.model.as_deref());

        let make_model = async {
            match &self.state.client_state {
                AnthropicClientState::Static(client) => Ok(client.completion(&model_name).erase()),
                AnthropicClientState::Dynamic {
                    token_provider,
                    base_url,
                    http_client,
                    auth_scope,
                } => {
                    // Token is acquired lazily on first request — the
                    // TokenProvider owns caching and refresh.
                    let wrapped = LazyAuthHttpClient::<BearerStrategy>::new(
                        http_client.clone(),
                        token_provider.clone(),
                        auth_scope.clone(),
                    );
                    let client = AnthropicConfig::new("unused")
                        .with_beta("compact-2026-09-04")
                        .with_base_url(base_url)
                        .connect(wrapped);
                    Ok(client.completion(&model_name).erase())
                }
            }
        };

        self.state
            .single_attempt(
                &params.prompt,
                ctx,
                cancel,
                interactive,
                crate::clients::native::PreserveReasoning::Yes,
                make_model,
            )
            .await
    }

    super::delegate_native_run_state!(state);

    fn supports_compaction(&self) -> bool {
        true
    }

    fn model(&self) -> &str {
        &self.state.model
    }

    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn compact(
        &self,
        messages: &[rig_core::completion::Message],
        context: &crate::clients::compaction::ResolvedRequestContext,
        instructions: Option<&str>,
    ) -> Result<Option<CompactionResult>, ClientError> {
        use http::StatusCode;
        use rig_core::http_client::HttpClientExt;
        use rig_core::wire::{Body, Mode, Operation, Wire};

        let driver = match &self.state.client_state {
            AnthropicClientState::Static(client) => client.completion(&context.model),
            AnthropicClientState::Dynamic {
                token_provider,
                base_url,
                http_client,
                auth_scope,
            } => {
                let wrapped = LazyAuthHttpClient::<BearerStrategy>::new(
                    http_client.clone(),
                    token_provider.clone(),
                    auth_scope.clone(),
                );
                AnthropicConfig::new("unused")
                    .with_beta("compact-2026-09-04")
                    .with_base_url(base_url)
                    .connect(wrapped)
                    .completion(&context.model)
            }
        };
        let mut context = context.clone();
        let mut compaction = serde_json::Map::new();
        compaction.insert("type".into(), "summarize".into());
        if let Some(inst) = instructions {
            compaction.insert("instructions".into(), inst.into());
        }
        let extra = context.extra.get_or_insert_with(|| serde_json::json!({}));
        let extra = extra.as_object_mut().ok_or_else(|| ClientError::Runtime {
            message: "Anthropic compaction provider parameters must be an object".into(),
        })?;
        extra.insert("compaction".into(), compaction.into());
        let request =
            <Completion as Operation>::prepare(context.request(messages), &driver.wire.describe())
                .map_err(|e| ClientError::Runtime {
                    message: format!("failed to prepare compaction request: {e}"),
                })?;
        let encoded =
            driver
                .wire
                .encode(request, Mode::Unary)
                .map_err(|e| ClientError::Runtime {
                    message: format!("failed to encode compaction request: {e}"),
                })?;
        let (parts, body) = encoded.request.into_parts();
        let Body::Bytes(body) = body else {
            return Err(ClientError::Runtime {
                message: "Anthropic compaction request must be JSON".into(),
            });
        };
        // Keep the concrete wire's endpoint, headers and HTTP version intact.
        let req = http::Request::from_parts(parts, body);
        let resp: http::Response<rig_core::http_client::LazyBody<Vec<u8>>> = driver
            .transport
            .send(req)
            .await
            .map_err(|e| ClientError::Runtime {
                message: format!("compaction HTTP request failed: {e}"),
            })?;

        if resp.status() != StatusCode::OK {
            let status = resp.status().as_u16();
            // Read (a truncated portion of) the error body so compaction
            // failures are diagnosable from logs.
            let body_snippet = match resp.into_body().await {
                Ok(data) => {
                    let text = String::from_utf8_lossy(&data);
                    if text.len() > 500 {
                        format!("{}…", &text[..500])
                    } else {
                        text.to_string()
                    }
                }
                Err(_) => String::from("(body unavailable)"),
            };
            return Err(ClientError::ApiServerError {
                message: format!("compaction returned HTTP {status}: {body_snippet}"),
            });
        }

        let body_bytes = resp.into_body().await.map_err(|e| ClientError::Runtime {
            message: format!("failed to read compaction response body: {e}"),
        })?;

        #[derive(serde::Deserialize)]
        struct CompactionContent {
            r#type: String,
            content: Option<String>,
            signature: Option<String>,
            #[serde(default)]
            encrypted_content: Option<String>,
            #[serde(default)]
            tool_changes: Option<Vec<serde_json::Value>>,
        }

        #[derive(serde::Deserialize)]
        struct CompactionResponse {
            stop_reason: Option<String>,
            content: Vec<CompactionContent>,
            usage: Option<CompactionUsageRaw>,
        }

        #[derive(serde::Deserialize)]
        struct CompactionUsageRaw {
            #[serde(default)]
            iterations: Vec<IterationUsage>,
        }

        #[derive(serde::Deserialize)]
        struct IterationUsage {
            #[serde(rename = "type", default)]
            r#type: String,
            #[serde(default)]
            input_tokens: Option<u64>,
            #[serde(default)]
            output_tokens: Option<u64>,
        }

        let resp_data: CompactionResponse =
            serde_json::from_slice(&body_bytes).map_err(|e| ClientError::Runtime {
                message: format!("failed to parse compaction response: {e}"),
            })?;

        if resp_data.stop_reason.as_deref() != Some("compaction") {
            return Err(ClientError::Runtime {
                message: format!(
                    "unexpected stop_reason in compaction response: {:?}",
                    resp_data.stop_reason
                ),
            });
        }

        // Extract compaction iteration usage: the top-level `usage` fields
        // are always zero for compaction calls; real cost is in iterations.
        let compaction_usage = resp_data
            .usage
            .as_ref()
            .and_then(|u| u.iterations.iter().find(|i| i.r#type == "compaction"));
        let input_tokens = compaction_usage.and_then(|u| u.input_tokens);
        let output_tokens = compaction_usage.and_then(|u| u.output_tokens);

        let block = resp_data
            .content
            .into_iter()
            .find(|c| c.r#type == "compaction");

        match block {
            Some(cc) => {
                // A null content block means no summary was produced
                // (e.g. max_tokens exceeded). Skip compaction for this
                // turn and retry later.
                let content = match cc.content {
                    Some(c) => c,
                    None => return Ok(None),
                };
                let signature = cc.signature.ok_or_else(|| ClientError::Runtime {
                    message: "compaction block missing signature".into(),
                })?;
                Ok(Some(CompactionResult {
                    block: Some(CompactionBlock {
                        content,
                        signature,
                        encrypted_content: cc.encrypted_content,
                        tool_changes: cc.tool_changes,
                    }),
                    input_tokens,
                    output_tokens,
                }))
            }
            None => Err(ClientError::Runtime {
                message: "no compaction block in response".into(),
            }),
        }
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
    use super::*;
    use crate::clients::cancel::CancelToken;
    use crate::test_support::EnvGuard;

    struct CompactToken(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl TokenProvider for CompactToken {
        async fn get_token(
            &self,
            scope: &str,
        ) -> Result<String, token_provider::TokenProviderError> {
            self.0.lock().unwrap().push(scope.into());
            Ok("fresh-compact-token".into())
        }
    }

    async fn compact_matches_stream_wire(dynamic: bool) {
        use rig_core::completion::message::{
            AssistantContent, AssistantMessage, Reasoning, ToolCall, ToolFunction,
        };
        use rig_core::completion::{Message, ToolDefinition};
        use rig_core::message::{Origin, ToolName};
        use rig_core::wire::{Body, Mode, Operation, Wire};
        use serde_json::json;
        use std::io::{BufRead, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/gateway", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                headers.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let fixture = json!({
                "stop_reason": "compaction",
                "content": [{"type": "compaction", "content": "server summary", "signature": "server signature", "encrypted_content": "sealed", "tool_changes": [{"name": "Do"}]}],
                "usage": {"input_tokens": 0, "output_tokens": 0, "iterations": [{"type": "compaction", "input_tokens": 123, "output_tokens": 17}]}
            }).to_string();
            write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", fixture.len(), fixture).unwrap();
            (
                headers,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        });
        let client = AnthropicConfig::new("configured-key")
            .with_base_url(&base)
            .with_beta("compact-2026-09-04")
            .connect(DynHttpClient::new(ReqwestClient::default()));
        let context = crate::clients::compaction::ResolvedRequestContext {
            model: "claude-sonnet-4-6".into(),
            system: Some("nondefault system framing".into()),
            tools: vec![ToolDefinition {
                name: ToolName::new("Do").unwrap(),
                description: "actual selected tool".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                annotations: Default::default(),
            }],
            extra: Some(
                json!({"cache_control": {"type": "ephemeral"}, "thinking": {"type": "enabled", "budget_tokens": 1024}, "compaction": {"type": "caller-value", "instructions": "caller instructions", "unexpected": true}}),
            ),
            temperature: Some(0.8),
            max_tokens: Some(8192),
        };
        let call = ToolCall::from_wire(
            "call-1",
            ToolFunction::new(ToolName::new("Do").unwrap(), json!({"path": "file"})),
        );
        let messages = vec![
            Message::user("prefix user"),
            Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(Reasoning::new("signed thought")).with_native(json!({"type": "thinking", "thinking": "signed thought", "signature": "native signature", "estimated_tokens": 42})),
                AssistantContent::ToolCall(call.clone()),
            ]).with_origin(Origin::new(rig_core::message::Api::from_static("anthropic.messages"), "anthropic", &context.model))),
            Message::tool_result(call.id, call.function.name, "tool output"),
        ];
        let driver = client.completion(&context.model);
        let prepared =
            <Completion as Operation>::prepare(context.request(&messages), &driver.wire.describe())
                .unwrap();
        let encoded = driver.wire.encode(prepared, Mode::Streaming).unwrap();
        let Body::Bytes(bytes) = encoded.request.body() else {
            panic!("JSON body")
        };
        let mut expected: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(expected["stream"], true);
        assert_eq!(expected["tool_choice"], json!({"type": "auto"}));
        // The Messages wire only inserts these streaming defaults in Streaming mode.
        expected.as_object_mut().unwrap().remove("stream");
        expected.as_object_mut().unwrap().remove("tool_choice");
        expected["compaction"] = json!({"type": "summarize", "instructions": "keep decisions"});
        let token = Arc::new(CompactToken(Default::default()));
        let state = if dynamic {
            AnthropicClientState::Dynamic {
                token_provider: token.clone(),
                base_url: base,
                http_client: DynHttpClient::new(ReqwestClient::default()),
                auth_scope: "compact-scope".into(),
            }
        } else {
            AnthropicClientState::Static(client)
        };
        let backend = AnthropicBackend {
            state: NativeRunState::new(
                state,
                "different-default-model".into(),
                None,
                AnthropicBackend::resolve_params(&Default::default()).unwrap(),
                "AnthropicBackend",
            ),
        };
        let result = backend
            .compact(&messages, &context, Some("keep decisions"))
            .await
            .unwrap()
            .unwrap();
        let (headers, body) = server.join().unwrap();
        assert_eq!(
            context.extra.as_ref().unwrap()["compaction"]["type"],
            "caller-value"
        );
        assert_eq!(body, expected);
        assert!(headers.starts_with("POST /gateway/v1/messages HTTP/1.1\r\n"));
        let headers = headers.to_lowercase();
        assert!(headers.contains("anthropic-version: 2023-06-01"));
        assert!(headers.contains("anthropic-beta: compact-2026-09-04"));
        assert!(headers.contains("content-type: application/json"));
        if dynamic {
            assert!(headers.contains("authorization: bearer fresh-compact-token"));
            assert!(!headers.contains("x-api-key:"));
            assert_eq!(*token.0.lock().unwrap(), vec!["compact-scope"]);
        } else {
            assert!(headers.contains("x-api-key: configured-key"));
        }
        assert_eq!(
            body["messages"][1]["content"][0]["signature"],
            "native signature"
        );
        assert!(!body.to_string().contains("estimated_tokens"));
        assert!(body.to_string().contains("tool_use"));
        assert!(body.to_string().contains("tool_result"));
        assert_eq!(result.input_tokens, Some(123));
        assert_eq!(result.output_tokens, Some(17));
        let block = result.block.unwrap();
        assert_eq!(block.content, "server summary");
        assert_eq!(block.signature, "server signature");
        assert_eq!(block.encrypted_content.as_deref(), Some("sealed"));
        assert_eq!(block.tool_changes, Some(vec![json!({"name": "Do"})]));
    }

    #[tokio::test]
    async fn production_compact_matches_stream_wire_static() {
        compact_matches_stream_wire(false).await;
    }

    #[tokio::test]
    async fn production_compact_matches_stream_wire_dynamic() {
        compact_matches_stream_wire(true).await;
    }

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
        let err = result.expect_err("should be an error");
        assert!(err.contains("no credentials for provider"), "got: {err}");
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

        let backend =
            AnthropicBackend::build("", &HashMap::new(), &indexmap::IndexMap::new()).unwrap();
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
        let err = result.expect_err("should be an error");
        assert!(err.contains("unknown auth value"), "got: {err}");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let client = Anthropic::new("sk-ant-test");

        let backend = AnthropicBackend {
            state: NativeRunState::new(
                AnthropicClientState::Static(client),
                "claude-sonnet-4-6".into(),
                None,
                ResolvedParams::default(),
                "test",
            ),
        };
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        let cancels = &backend.state.cancels;
        cancels.insert("gr-test", 1, a.clone()).unwrap();
        cancels.insert("gr-test", 2, b.clone()).unwrap();
        cancels.insert("gr-sibling", 3, sibling.clone()).unwrap();
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

        let backend =
            AnthropicBackend::build_concrete("claude-sonnet-4-6", &HashMap::new(), &extra).unwrap();
        assert_eq!(backend.state.resolved.max_tokens, Some(8192));
    }

    /// A malformed `max_tokens` value (not a valid u64) is a build error.
    #[test]
    fn build_max_tokens_malformed_is_error() {
        let mut guard = isolated_env();
        guard.set("ANTHROPIC_API_KEY", "sk-ant-test");

        let mut extra = indexmap::IndexMap::new();
        extra.insert("max_tokens".into(), "not-a-number".into());

        let err = AnthropicBackend::build("claude-sonnet-4-6", &HashMap::new(), &extra)
            .expect_err("malformed max_tokens must error");
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

        let backend =
            AnthropicBackend::build_concrete("claude-sonnet-4-6", &HashMap::new(), &extra).unwrap();

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
        let model = backend.build_model("anthropic/claude-sonnet-4-6");
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
        let model = backend.build_model("anthropic/claude-sonnet-4-6");
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
        assert!(backend.build_model("openai/gpt-4o").is_none());
        assert!(backend.build_model("azure-openai/gpt-4o").is_none());
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

        let backend =
            AnthropicBackend::build_concrete("claude-sonnet-4-6", &HashMap::new(), &extra).unwrap();

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
