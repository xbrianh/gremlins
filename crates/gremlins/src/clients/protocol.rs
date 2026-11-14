use serde::{Deserialize, Serialize};

use super::conversation::Conversation;
use rig_core::completion::Message;

pub use gremlins_magic_bus::stats::{add_opt, TurnStats, UsageStats};

/// A signed compaction block from the Anthropic on-demand compaction API
/// (`compact-2026-09-04`). Round-tripped verbatim in subsequent requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionBlock {
    pub content: String,
    pub signature: String,
    #[serde(default)]
    pub encrypted_content: Option<String>,
    #[serde(default)]
    pub tool_changes: Option<Vec<serde_json::Value>>,
}

/// The result of a successful server-side compaction call: the signed block
/// plus optional token usage from the compaction API call itself (reported
/// in `usage.iterations` rather than the top-level `usage` fields).
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub block: Option<CompactionBlock>,
    /// Input tokens consumed by the compaction summarization.
    pub input_tokens: Option<u64>,
    /// Output tokens produced by the compaction summarization.
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletedRun {
    pub exit_code: i32,
    pub text_result: Option<String>,
    /// The `Done` tool's `summary` argument, preserved distinctly from
    /// `text_result` so the chat TUI can append it at turn end.
    pub summary: Option<String>,
    pub events: Option<Vec<serde_json::Value>>,
    pub cost_usd: Option<f64>,
    pub token_usage: Option<UsageStats>,
    /// The agent loop's final conversation, when the backend captures it.
    /// Used by the chat gremlin to persist full model context between stages
    /// via `artifact://conversation.json`. The `messages()` method provides
    /// the serializable transcript; the trait object itself is never serialized.
    #[serde(skip)]
    pub(crate) conversation: Option<Box<dyn Conversation>>,
}

impl CompletedRun {
    /// The agent loop's final durable message transcript.
    /// `None` for non-agent backends (e.g. `cmd`, `exec`).
    pub fn messages(&self) -> Option<Vec<Message>> {
        self.conversation.as_ref().map(|conv| conv.messages())
    }
}

// Manual Clone impl needed because `Box<dyn Conversation>` doesn't
// auto-derive Clone — we use the trait's `clone_box()` method.
impl Clone for CompletedRun {
    fn clone(&self) -> Self {
        Self {
            exit_code: self.exit_code,
            text_result: self.text_result.clone(),
            summary: self.summary.clone(),
            events: self.events.clone(),
            cost_usd: self.cost_usd,
            token_usage: self.token_usage.clone(),
            conversation: self.conversation.clone(),
        }
    }
}
