use serde::{Deserialize, Serialize};

use rig_core::completion::Message;

/// Aggregated token usage summary from an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub turns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedRun {
    pub exit_code: i32,
    pub text_result: Option<String>,
    /// The `Done` tool's `summary` argument, preserved distinctly from
    /// `text_result` so the chat TUI can append it at turn end.
    pub summary: Option<String>,
    pub events: Option<Vec<serde_json::Value>>,
    pub cost_usd: Option<f64>,
    pub token_usage: Option<UsageStats>,
    /// The agent loop's accumulated message history (all roles), when the
    /// backend captures it. Used by the chat gremlin to persist full model
    /// context between stages via `artifact://conversation.json`.
    pub messages: Option<Vec<Message>>,
}
