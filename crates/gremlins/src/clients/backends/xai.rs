use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::openai_protocol::{self, reap_openai_compat, run_openai_compat, OpenAiRunState};
use crate::clients::protocol::CompletedRun;

const PROVIDER_NAME: &str = "xai";
const API_KEY_ENV: &str = "XAI_API_KEY";
const BASE_URL: &str = "https://api.x.ai/v1";
const DEFAULT_MODEL: &str = "grok-4";

pub struct XaiBackend {
    state: OpenAiRunState,
}

impl XaiBackend {
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
                "XaiBackend".to_string(),
            ),
        }
    }

    /// Build an xAI backend. Resolves `XAI_API_KEY` → `providers.json`.
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
impl Backend for XaiBackend {
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
    use super::*;

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "xai");
        assert_eq!(API_KEY_ENV, "XAI_API_KEY");
        assert_eq!(BASE_URL, "https://api.x.ai/v1");
        assert_eq!(DEFAULT_MODEL, "grok-4");
    }
}