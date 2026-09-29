use std::collections::HashMap;

use async_trait::async_trait;
use rig_core::providers::openai;

use super::backend::{Backend, ClientError, RunParams};
use super::openai_protocol::{reap_openai_compat, run_openai_compat, OpenAiRunState};
use super::protocol::CompletedRun;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiProvider {
    OpenAi,
    Xai,
}

impl OpenAiProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Xai => "xai",
        }
    }

    pub fn api_key_env(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Xai => "XAI_API_KEY",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Xai => "https://api.x.ai/v1",
        }
    }

    pub(crate) fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o",
            Self::Xai => "grok-4",
        }
    }
}

pub struct OpenAiBackend {
    provider: OpenAiProvider,
    state: OpenAiRunState,
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
            provider,
            state: OpenAiRunState::new(
                client,
                model,
                tool_filter,
                client_params,
                "OpenAiBackend".to_string(),
            ),
        }
    }
}

#[async_trait]
impl Backend for OpenAiBackend {
    async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError> {
        run_openai_compat(&self.state, params, None, self.provider.name()).await
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
    use super::super::agent_loop::CancelToken;
    use super::*;

    #[test]
    fn provider_identity() {
        assert_eq!(OpenAiProvider::OpenAi.name(), "openai");
        assert_eq!(OpenAiProvider::Xai.name(), "xai");
        assert_eq!(OpenAiProvider::OpenAi.api_key_env(), "OPENAI_API_KEY");
        assert_eq!(OpenAiProvider::Xai.api_key_env(), "XAI_API_KEY");
        assert_eq!(OpenAiProvider::Xai.base_url(), "https://api.x.ai/v1");
        assert_eq!(OpenAiProvider::Xai.default_model(), "grok-4");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
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
        // The gremlin entry is dropped when empty.
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-test")
            .is_none());
        // Sibling tokens are untouched.
        assert!(!sibling.is_cancelled());
        assert!(backend
            .state
            .cancels
            .lock()
            .unwrap()
            .get("gr-sibling")
            .is_some());
    }
}
