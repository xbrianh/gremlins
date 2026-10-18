//! Backend implementations and the provider registry.
//!
//! Each backend lives in its own file and exports a `build` function matching
//! [`BuildFn`]. The [`backends!`] macro generates both the `pub mod`
//! declarations and a [`registry`] function that maps provider names to their
//! constructors — adding a backend is a single-line edit in the macro
//! invocation below.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::clients::backend::Backend;

/// The signature every provider's `build` function must match.
pub type BuildFn = fn(
    model: &str,
    native_block: &HashMap<String, Vec<String>>,
    extra_params: &IndexMap<String, String>,
) -> Result<Arc<dyn Backend>, String>;

/// Declare an OpenAI-protocol backend.
///
/// OpenAI, OpenRouter, and xAI share the exact same shape: a thin wrapper
/// around [`crate::clients::openai_protocol::OpenAiRunState`] plus a `build`
/// constructor and a [`Backend`] impl that delegates to the shared protocol
/// helpers.  This macro generates all of that from a handful of per-provider
/// constants, leaving each backend module as constants plus one invocation.
///
/// The classifier expression is either `None` or `Some(path::to::fn)`; the
/// generated `run` coerces it to `Option<ErrorClassifier>`.
macro_rules! declare_openai_backend {
    (
        struct $backend:ident,
        provider: $provider:expr,
        api_key_env: $api_key_env:expr,
        base_url_env: $base_url_env:expr,
        base_url: $base_url:expr,
        default_model: $default_model:expr,
        dialect: $dialect:path,
        classifier: $classifier:expr $(,)?
    ) => {
        /// OpenAI-protocol backend wrapper.
        pub struct $backend {
            state: $crate::clients::openai_protocol::OpenAiRunState,
        }

        impl $backend {
            /// Build this backend. Resolves the provider API key env var
            /// first, then falls back to `providers.yaml`.
            pub fn build(
                model: &str,
                native_block: &std::collections::HashMap<String, Vec<String>>,
                extra_params: &indexmap::IndexMap<String, String>,
            ) -> Result<std::sync::Arc<dyn $crate::clients::backend::Backend>, String> {
                let key =
                    $crate::clients::config::api_key($api_key_env, $provider).ok_or_else(|| {
                        format!(
                            "no API key for provider '{}': set {} or add an entry in {}",
                            $provider,
                            $api_key_env,
                            $crate::config::user_config_root()
                                .join("providers.yaml")
                                .display(),
                        )
                    })?;
                let base_url =
                    $crate::clients::config::base_url($base_url_env, $provider, $base_url);
                let client = $crate::clients::openai_protocol::build_openai_client(
                    &key, &base_url, &$dialect,
                )?;
                let model = if model.is_empty() {
                    $default_model.to_string()
                } else {
                    model.to_string()
                };
                Ok(std::sync::Arc::new(Self {
                    state: $crate::clients::openai_protocol::OpenAiRunState::new(
                        client,
                        model,
                        $crate::clients::openai_protocol::tool_filter(native_block),
                        <Self as $crate::clients::backend::Backend>::resolve_params(extra_params)?,
                        stringify!($backend).to_string(),
                    ),
                }))
            }
        }

        #[async_trait::async_trait]
        impl $crate::clients::backend::Backend for $backend {
            async fn run(
                &self,
                params: $crate::clients::backend::RunParams,
                interactive: Option<$crate::clients::interactive::InteractiveSession>,
            ) -> Result<
                $crate::clients::protocol::CompletedRun,
                $crate::clients::backend::ClientError,
            > {
                let classify: Option<$crate::clients::agent_loop::ErrorClassifier> = $classifier;
                $crate::clients::openai_protocol::run_openai_compat(
                    &self.state,
                    params,
                    interactive,
                    classify,
                    $provider,
                )
                .await
            }

            fn last_run_params(
                &self,
            ) -> Result<$crate::clients::backend::RunParams, $crate::clients::backend::ClientError>
            {
                self.state.last_run_params()
            }

            fn reap_all(&self, gremlin_id: &str) {
                $crate::clients::openai_protocol::reap_openai_compat(&self.state, gremlin_id);
            }

            fn total_cost_usd(&self) -> Option<f64> {
                None
            }

            fn resolved_params(&self) -> Option<$crate::clients::params::ResolvedParams> {
                Some(self.state.resolved.clone())
            }

            fn make_model(&self, spec: &str) -> Option<$crate::clients::model::GremlinsModel> {
                $crate::clients::openai_protocol::make_model(&self.state, $provider, spec)
            }

            fn resolve_params(
                raw: &indexmap::IndexMap<String, String>,
            ) -> Result<$crate::clients::params::ResolvedParams, String> {
                $crate::clients::openai_protocol::resolve_openai_protocol($provider, raw)
            }
        }
    };
}
pub(crate) use declare_openai_backend;

/// Declare backend modules and generate a static registry.
///
/// Each entry is `module: "provider_name" => path::to::Struct::build`.
/// The macro expands to `pub mod module;` for each entry and a single
/// `registry()` function returning `Vec<(&'static str, BuildFn)>`.
macro_rules! backends {
    ($($module:ident : $name:literal => $build:path),* $(,)?) => {
        $(pub mod $module;)*

        /// Return every registered backend, in declaration order.
        pub fn registry() -> Vec<(&'static str, BuildFn)> {
            vec![$(
                ($name, $build as BuildFn),
            )*]
        }
    };
}

backends! {
    cmd: "cmd" => cmd::CmdBackend::build,
    copilot: "copilot" => copilot::CopilotBackend::build,
    openai: "openai" => openai::OpenAiBackend::build,
    xai: "xai" => xai::XaiBackend::build,
    openrouter: "openrouter" => openrouter::OpenRouterBackend::build,
    anthropic: "anthropic" => anthropic::AnthropicBackend::build,
    azure_openai: "azure-openai" => azure_openai::AzureOpenAiBackend::build,
}
