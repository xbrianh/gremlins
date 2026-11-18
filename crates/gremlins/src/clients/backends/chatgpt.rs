//! ChatGPT subscription backend using rig's Responses SSE dialect.

mod oauth;

mod catalog;
pub(super) use catalog::list_models;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use rig_core::driver::DynModel;
use rig_core::http_client::DynHttpClient;
use rig_core::operation::Completion;
use rig_core::providers::chatgpt;
use rig_core::providers::openai::{OpenAI, OpenAIConfig};

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::cancel::CancelToken;
#[cfg(test)]
use crate::clients::chatgpt_auth::ACCESS_TOKEN_ENV;
use crate::clients::chatgpt_auth::{static_source, stored_oauth};
use crate::clients::config::tool_filter;
use crate::clients::interactive::InteractiveSession;
use crate::clients::native::{NativeRunState, PreserveReasoning};
use crate::clients::params::{self, ResolvedParams};
use crate::clients::protocol::CompletedRun;
use crate::model_uri::provider_and_model;

const PROVIDER_NAME: &str = "chatgpt";

/// Where model credentials come from.
enum Credentials {
    /// A prebuilt client over a static token.
    Static(Box<OpenAI>),
    /// A store for model selection and a transport that resolves it on each request.
    OAuth(oauth::StoredOAuth),
}

struct ChatGptRunState {
    credentials: Credentials,
    native: NativeRunState<()>,
}

pub(super) struct ChatGptBackend {
    state: ChatGptRunState,
}

impl ChatGptBackend {
    pub(super) fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        Self::build_with(model, native_block, extra_params, rig_reqwest::shared())
    }

    fn build_with(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &IndexMap<String, String>,
        http: DynHttpClient,
    ) -> Result<Arc<dyn Backend>, String> {
        let credentials = match static_source() {
            Some(found) => Credentials::Static(Box::new(
                OpenAIConfig::with_key(&chatgpt::DIALECT, found.secret).connect(http),
            )),
            None => {
                let store = stored_oauth().map_err(|error| error.to_string())?;
                Credentials::OAuth(oauth::StoredOAuth { store, http })
            }
        };
        let model = if model.is_empty() {
            chatgpt::DEFAULT_MODEL
        } else {
            model
        };
        Ok(Arc::new(Self {
            state: ChatGptRunState {
                credentials,
                native: NativeRunState::new(
                    (),
                    model.to_owned(),
                    tool_filter(native_block),
                    Self::resolve_params(extra_params)?,
                    "ChatGptBackend",
                ),
            },
        }))
    }
}

impl ChatGptBackend {
    /// Check the sign-in during selection so a broken Task override fails early.
    /// Only the API base is needed here; the transport supplies request credentials.
    async fn model_for(&self, name: &str) -> Result<DynModel<Completion>, ClientError> {
        let oauth = match &self.state.credentials {
            Credentials::Static(client) => return Ok(client.completion(name).erase()),
            Credentials::OAuth(oauth) => oauth,
        };
        let context = oauth.resolve()
            .await
            .map_err(|error| ClientError::Runtime {
                message: error.to_string(),
            })?;
        let mut config = OpenAIConfig::with_key(&chatgpt::DIALECT, "");
        if let Some(api_base) = context.api_base {
            config = config.with_base_url(api_base);
        }
        let http = oauth.http.clone().with_middleware(oauth.clone());
        Ok(config.connect(http).completion(name).erase())
    }
}

#[async_trait]
impl Backend for ChatGptBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        let native = &self.state.native;
        let ctx = native.prepare_context(params.clone());
        let cancel = params.cancel_token.unwrap_or_else(CancelToken::new);
        let model_name = native.effective_model(ctx.params.model.as_deref());
        native
            .single_attempt(
                &params.prompt,
                ctx,
                cancel,
                interactive,
                PreserveReasoning::No,
                self.model_for(&model_name),
            )
            .await
    }

    super::delegate_native_run_state!(state.native);

    /// Synchronous construction only serves static credentials; OAuth mode must
    /// resolve asynchronously, so callers go through [`Backend::make_model`].
    fn build_model(&self, spec: &str) -> Option<DynModel<Completion>> {
        let (provider, model) = provider_and_model(spec)?;
        match &self.state.credentials {
            Credentials::Static(client) if provider == PROVIDER_NAME => {
                Some(client.completion(&model).erase())
            }
            _ => None,
        }
    }

    async fn make_model(&self, spec: &str) -> Result<Option<DynModel<Completion>>, ClientError> {
        match provider_and_model(spec) {
            Some((provider, model)) if provider == PROVIDER_NAME => {
                self.model_for(&model).await.map(Some)
            }
            _ => Ok(None),
        }
    }

    fn model(&self) -> &str {
        &self.state.native.model
    }

    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn resolve_params(raw: &IndexMap<String, String>) -> Result<ResolvedParams, String> {
        let mut resolved = ResolvedParams::default();
        for (key, value) in raw {
            match key.as_str() {
                "temperature" => {
                    resolved.temperature = Some(params::parse_f64(PROVIDER_NAME, key, value)?)
                }
                "max_tokens" => {
                    resolved.max_tokens = Some(params::parse_u64(PROVIDER_NAME, key, value)?)
                }
                _ => {
                    resolved
                        .additional
                        .insert(key.clone(), params::coerce(value));
                }
            }
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod run_test;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::client::is_known_provider;
    use crate::test_support::EnvGuard;
    use serde_json::json;

    #[test]
    fn builds_from_env_and_routes_task_models() {
        let mut env = EnvGuard::lock();
        env.set(ACCESS_TOKEN_ENV, "test-access-token");
        let backend = ChatGptBackend::build("", &HashMap::new(), &IndexMap::new()).unwrap();
        assert!(is_known_provider("chatgpt"));
        assert_eq!(backend.name(), "chatgpt");
        assert_eq!(backend.model(), chatgpt::DEFAULT_MODEL);
        assert!(backend.build_model("chatgpt/gpt-5.3-codex").is_some());
        assert!(backend.build_model("openai/gpt-4o").is_none());
    }

    #[test]
    fn file_credentials_require_a_token_not_an_api_key() {
        let mut env = EnvGuard::lock();
        let root = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", root.path());
        env.remove(ACCESS_TOKEN_ENV);
        env.set("CHATGPT_API_KEY", "not-an-access-token");
        let config_dir = root.path().join("config");
        std::fs::create_dir(&config_dir).unwrap();
        let path = config_dir.join("providers.yaml");
        let build = || ChatGptBackend::build("gpt-5.4", &HashMap::new(), &IndexMap::new());
        std::fs::write(&path, "chatgpt:\n  api-key: not-an-access-token\n").unwrap();
        let Err(error) = build() else {
            panic!("expected missing credentials");
        };
        assert_eq!(
            error,
            "no credentials for provider 'chatgpt': set CHATGPT_ACCESS_TOKEN, \
             add chatgpt.token in providers.yaml, or run `gremlins auth chatgpt login`"
        );
        std::fs::write(path, "chatgpt:\n  token: test-access-token\n").unwrap();
        assert_eq!(build().unwrap().model(), "gpt-5.4");
    }

    #[test]
    fn resolves_typed_fields_and_forwards_other_params() {
        let raw = IndexMap::from([
            ("temperature".into(), "0.5".into()),
            ("max_tokens".into(), "2048".into()),
            ("reasoning".into(), r#"{"effort":"high"}"#.into()),
        ]);
        let resolved = ChatGptBackend::resolve_params(&raw).unwrap();
        assert_eq!(resolved.temperature, Some(0.5));
        assert_eq!(resolved.max_tokens, Some(2048));
        assert_eq!(
            resolved.additional_value(),
            Some(json!({"reasoning": {"effort": "high"}}))
        );
    }

    #[test]
    fn rejects_malformed_typed_fields() {
        for (key, value) in [("temperature", "hot"), ("max_tokens", "-1")] {
            let raw = IndexMap::from([(key.into(), value.into())]);
            let error = ChatGptBackend::resolve_params(&raw).unwrap_err();
            assert!(error.contains(key), "{error}");
        }
    }
}
