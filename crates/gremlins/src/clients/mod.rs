pub(crate) mod agent;

/// Shared log-line emitter type used by the agent loop and tool runtime.
pub(crate) type LogLineFn = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Async stream-event emitter. Called from the agent loop to send stream
/// events (StreamChunk, ReasoningChunk, ToolOutputChunk, ToolResult,
/// TurnComplete, Done, StreamReset, Ready, Ended). The executor wires this to
/// the run-scoped bus emitter.
pub(crate) type StreamEventFn = std::sync::Arc<
    dyn Fn(
            gremlins_magic_bus::EventKind,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

pub(crate) mod anthropic_bearer_http;
pub(crate) mod azure_openai_responses_http;
pub mod backend;
pub mod backends;
pub(crate) mod cancel;
pub(crate) mod cancels;
pub mod client;
pub(crate) use client::run_naked;
pub(crate) mod compaction;
pub mod config;
pub mod conversation;
pub(crate) mod error;
pub(crate) mod format;
pub mod github_auth;
pub(crate) mod interactive;
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
