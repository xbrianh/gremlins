#![deny(unreachable_pub)]
#![allow(clippy::double_must_use)]
pub mod artifacts;
pub mod builders;
pub mod clients;
pub mod config;
pub(crate) mod conversation;
pub mod core;
pub mod definition;
pub mod executor;
pub use gremlins_landscape as landscape;
pub mod model_uri;
pub mod prelude;
pub mod schemas;
pub mod skills;
pub mod stage_spec;
pub(crate) mod task_kind;

/// Build version shared by CLI and daemon.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Factory: build a boxed [`StandardConversation`] so callers outside
/// `conversation/` never spell the concrete type.
pub(crate) fn new_conversation(
    prompt: &str,
    preserve_reasoning: bool,
    prepopulated_messages: Option<Vec<rig_core::completion::Message>>,
    compaction: Option<crate::clients::compaction::CompactionConfig>,
) -> Box<dyn crate::clients::conversation::Conversation> {
    Box::new(crate::conversation::standard::StandardConversation::new(
        prompt,
        preserve_reasoning,
        prepopulated_messages,
        compaction,
    ))
}

#[cfg(test)]
mod test_support;
