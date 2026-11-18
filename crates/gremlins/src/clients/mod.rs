pub(crate) mod agent;

pub(crate) mod anthropic_bearer_http;
pub(crate) mod azure_openai_responses_http;
pub mod backend;
pub mod backends;
pub(crate) mod cancel;
pub(crate) mod cancels;
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
    backends::registry().into_iter().map(|(name, _)| name)
}
