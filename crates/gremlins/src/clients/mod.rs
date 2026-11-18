pub(crate) mod agent;

pub(crate) mod anthropic_bearer_http;
pub(crate) mod azure_openai_responses_http;
pub mod backend;
pub mod backends;
pub(crate) mod cancel;
pub(crate) mod cancels;
pub mod chatgpt_auth;
pub mod client;
pub(crate) mod path_context;
pub(crate) use client::run_naked;
pub(crate) mod compaction;
pub mod config;
pub mod conversation;
pub(crate) mod error;
pub(crate) mod format;
pub mod github_auth;
pub(crate) mod interactive;
pub use interactive::InteractiveSession;
pub(crate) mod lazy_auth_http;
pub(crate) mod naked_conversation;
pub(crate) mod native;
pub mod openai_protocol;
pub mod params;
pub mod protocol;
pub(crate) mod shell_parse;
pub(crate) mod stream_json;
pub(crate) mod token_provider;
pub(crate) mod tools;

/// Names accepted in model specs, in backend registration order.
pub fn provider_names() -> impl Iterator<Item = &'static str> {
    backends::registry().into_iter().map(|entry| entry.name)
}

/// Why model discovery produced no list.
#[derive(Debug)]
pub enum ListModelsError {
    /// No backend has this name.
    Unknown { registered: Vec<&'static str> },
    /// The backend is registered but cannot enumerate its models.
    Unsupported { backend: &'static str },
    /// Discovery ran and failed.
    Failed(String),
}

/// List the models `backend` offers the current account, as canonical URIs.
///
/// The backend is resolved before any credential lookup.
pub async fn list_models(
    backend: &str,
) -> Result<Vec<crate::model_uri::ModelURI>, ListModelsError> {
    let registry = backends::registry();
    let Some(entry) = registry.iter().find(|entry| entry.name == backend) else {
        return Err(ListModelsError::Unknown {
            registered: registry.iter().map(|entry| entry.name).collect(),
        });
    };
    let Some(list) = entry.list_models else {
        return Err(ListModelsError::Unsupported {
            backend: entry.name,
        });
    };
    let slugs = list().await.map_err(ListModelsError::Failed)?;
    slugs
        .into_iter()
        .map(|slug| {
            crate::model_uri::ModelURI::from_parts(entry.name.to_string(), slug, Default::default())
                .map_err(ListModelsError::Failed)
        })
        .collect()
}
