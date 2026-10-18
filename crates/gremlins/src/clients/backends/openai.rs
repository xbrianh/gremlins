use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use rig_core::providers::openai::{OpenAI, wire};

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::model::GremlinsModel;
use crate::clients::openai_protocol::{self, reap_openai_compat, run_openai_compat, OpenAiRunState};
use crate::clients::params::ResolvedParams;
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
        client: OpenAI,
        model: String,
        tool_filter: Option<Vec<String>>,
        resolved: ResolvedParams,
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
                resolved,
                "OpenAiBackend".to_string(),
            ),
        }
    }

    /// Build an OpenAI backend. Resolves `OPENAI_API_KEY` → `providers.yaml`.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let key =
            crate::clients::config::api_key(API_KEY_ENV, PROVIDER_NAME).ok_or_else(|| {
                format!(
                    "no API key for provider '{PROVIDER_NAME}': set {API_KEY_ENV} or add an entry in {}",
                    crate::config::user_config_root()
                        .join("providers.yaml")
                        .display(),
                )
            })?;
        let base_url = crate::clients::config::base_url("OPENAI_BASE_URL", PROVIDER_NAME, BASE_URL);
        let client = openai_protocol::build_openai_client(&key, &base_url, &wire::OPENAI)?;
        Ok(Arc::new(Self::new(
            client,
            model.to_string(),
            openai_protocol::tool_filter(native_block),
            Self::resolve_params(extra_params)?,
        )))
    }
}

#[async_trait]
impl Backend for OpenAiBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: Option<InteractiveSession>,
    ) -> Result<CompletedRun, ClientError> {
        run_openai_compat(&self.state, params, interactive, None, PROVIDER_NAME).await
    }

    fn last_run_params(&self) -> Result<RunParams, ClientError> {
        self.state.last_run_params()
    }

    fn reap_all(&self, gremlin_id: &str) {
        reap_openai_compat(&self.state, gremlin_id);
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }

    fn resolved_params(&self) -> Option<ResolvedParams> {
        Some(self.state.resolved.clone())
    }

    fn make_model(&self, spec: &str) -> Option<GremlinsModel> {
        let (provider, model) = openai_protocol::provider_and_model(spec)?;
        if provider == PROVIDER_NAME {
            Some(GremlinsModel::new(
                self.state.client.completion(&model).erase(),
                crate::config::max_prompt_bytes(),
            ))
        } else {
            None
        }
    }

    fn resolve_params(raw: &indexmap::IndexMap<String, String>) -> Result<ResolvedParams, String> {
        openai_protocol::resolve_openai_protocol(PROVIDER_NAME, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::agent_loop::CancelToken;
    use super::*;
    use crate::clients::cancels::CancelsMap;

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "openai");
        assert_eq!(API_KEY_ENV, "OPENAI_API_KEY");
        assert_eq!(BASE_URL, "https://api.openai.com/v1");
        assert_eq!(DEFAULT_MODEL, "gpt-4o");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let client = OpenAI::new("sk-test");
        let backlog = CancelsMap::default();
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        backlog.insert("gr-test", 1, a.clone()).unwrap();
        backlog.insert("gr-test", 2, b.clone()).unwrap();
        backlog.insert("gr-sibling", 3, sibling.clone()).unwrap();

        let mut backend = OpenAiBackend::new(client, "gpt-4o".into(), None, ResolvedParams::default());
        backend.state.cancels = backlog;
        backend.reap_all("gr-test");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(!sibling.is_cancelled());
    }
}