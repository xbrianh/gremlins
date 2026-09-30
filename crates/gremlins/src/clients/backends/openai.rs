use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::openai_protocol::{self, reap_openai_compat, run_openai_compat, OpenAiRunState};
use crate::clients::protocol::CompletedRun;

const PROVIDER_NAME: &str = "openai";
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o";

pub struct OpenAiBackend {
    state: OpenAiRunState,
}

impl OpenAiBackend {
    pub fn new(
        client: rig_core::providers::openai::CompletionsClient,
        model: String,
        tool_filter: Option<Vec<String>>,
        client_params: HashMap<String, String>,
    ) -> Self {
        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model
        };
        Self {
            state: OpenAiRunState::new(
                client,
                model,
                tool_filter,
                client_params,
                "OpenAiBackend".to_string(),
            ),
        }
    }

    /// Build an OpenAI backend. Resolves `OPENAI_API_KEY` → `providers.json`.
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
        Ok(Arc::new(Self::new(
            client,
            model.to_string(),
            openai_protocol::tool_filter(native_block),
            openai_protocol::string_map(extra_params),
        )))
    }
}

#[async_trait]
impl Backend for OpenAiBackend {
    async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError> {
        run_openai_compat(&self.state, params, None, PROVIDER_NAME).await
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
    use super::super::super::agent_loop::CancelToken;
    use super::*;

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "openai");
        assert_eq!(API_KEY_ENV, "OPENAI_API_KEY");
        assert_eq!(BASE_URL, "https://api.openai.com/v1");
        assert_eq!(DEFAULT_MODEL, "gpt-4o");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let client = rig_core::providers::openai::Client::builder()
            .api_key(rig_core::client::BearerAuth::from("sk-test"))
            .base_url("https://api.openai.com/v1")
            .build()
            .unwrap()
            .completions_api();
        let backend = OpenAiBackend::new(client, "gpt-4o".into(), None, HashMap::new());
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