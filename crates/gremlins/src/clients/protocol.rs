use serde::{Deserialize, Serialize};

use rig_core::completion::Message;

/// Aggregated token usage summary from an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    pub turns: usize,
}

/// One turn's token/cache delta.
///
/// Field names intentionally mirror [`UsageStats`] so chat consumers see the
/// same canonical wire keys for both per-turn and aggregate usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStats {
    /// The agent-loop turn index that produced this usage.
    pub turn: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
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
