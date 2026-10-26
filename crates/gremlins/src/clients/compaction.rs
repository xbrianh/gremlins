//! Pure types and helpers for conversation compaction.
//!
//! The concrete compaction algorithm lives in
//! [`crate::executor::conversation`] behind the [`Compactor`] trait,
//! supplied to the agent loop via [`super::backend::RunParams`].  This
//! avoids a heavyweight layering violation where the clients transport
//! layer would otherwise transitively import the full executor
//! conversation module just to know about
//! [`CutGranularity`] / [`CompactedWorking`] and the byte-token helpers.

use rig_core::completion::message::{CallId, Text, ToolName, ToolResultContent, UserContent};
use rig_core::completion::{Message, ToolDefinition};
use rig_core::message::ToolResult as ToolResultMsg;
use std::sync::OnceLock;

/// Compaction tuning. `threshold_tokens` is the soft token trigger; when a
/// request view exceeds it the loop compacts toward `target_tokens` so one
/// pass buys many rounds before the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactionOpts {
    pub(crate) threshold_tokens: u64,
    pub(crate) target_tokens: u64,
}

/// Conservative bytes-per-token ratio used to convert a token budget
/// into a byte budget.  Dense-content assumption: fitting to this byte
/// ceiling under-shoots the token target.
pub(crate) const BYTES_PER_TOKEN: u64 = 3;

/// Where a compaction pass is allowed to cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CutGranularity {
    /// Cut only at the start of a completed chat turn (read-time default).
    Turn,
    /// Cut at round starts inside the current turn, plus chat-turn starts
    /// before it.  The current turn's operator prompt and any
    /// `[operator]:` injections are *not* cut; they are carried verbatim
    /// ahead of the window.
    Round { current_turn_start: usize },
}

/// A compacted working view split into a stable leading prefix and the
/// verbatim tail that starts at `cut` in the source history.
#[derive(Clone, Debug)]
pub(crate) struct CompactedWorking {
    pub(crate) prefix: Vec<Message>,
    pub(crate) cut: usize,
}

impl CompactedWorking {
    /// Rebuild the full working view for the current source history.
    pub(crate) fn materialize(&self, history: &[Message]) -> Vec<Message> {
        if self.cut > history.len() {
            return history.to_vec();
        }
        let mut working = self.prefix.clone();
        working.extend_from_slice(&history[self.cut..]);
        working
    }
}

/// The single algorithm entry-point, supplied by the executor layer.
pub(crate) trait Compactor: Send + Sync {
    fn compact_working(
        &self,
        history: &[Message],
        budget_bytes: u64,
        request_overhead_bytes: usize,
        granularity: CutGranularity,
    ) -> Option<CompactedWorking>;
}

// ---------------------------------------------------------------------------
// Byte / token helpers used by both the concrete algorithm and the agent loop
// ---------------------------------------------------------------------------

/// Classify a tool result string as `"ok"` or `"error"`.
pub(crate) fn result_status(res: &str) -> &'static str {
    if res.starts_with("Error:") || res.starts_with("[exit") || res.starts_with("[timeout]") {
        "error"
    } else {
        "ok"
    }
}

/// Build a single [`Message::User`] that merges a Done action ledger with
/// the matching tool_result. This avoids consecutive `User` messages in
/// the persisted history, which Anthropic (and other providers) reject
/// because they violate the required user/assistant alternation and the
/// rule that every `tool_use` must be immediately followed by a matching
/// `tool_result` block.
///
/// The [`UserContent::ToolResult`] comes first because Anthropic requires
/// every `tool_result` block to precede any text blocks in the responding
/// user message.
pub(crate) fn done_user_message(
    ledger: &str,
    call_id: CallId,
    tool_name: ToolName,
    result_text: &str,
) -> Message {
    Message::User {
        content: vec![
            UserContent::ToolResult(ToolResultMsg {
                call: call_id,
                name: tool_name,
                content: vec![ToolResultContent::Text(Text {
                    text: result_text.to_string(),
                    additional_params: None,
                })],
            }),
            UserContent::Text(Text {
                text: ledger.to_string(),
                additional_params: None,
            }),
        ],
    }
}

/// Estimate the serialized size of every request field except the chat
/// history.
///
/// The agent loop's hard prompt ceiling measures the normalized
/// [`rig_core::completion::CompletionRequest`], not the bare transcript. This
/// helper builds that same request with an empty message list and subtracts
/// the one-byte difference between an empty and a non-empty `chat_history`
/// array; the compaction algorithm then adds the serialized history length
/// back.
pub(crate) fn completion_request_overhead(
    prompt: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) -> usize {
    let mut request =
        rig_core::completion::CompletionRequest::new(Message::user(prompt.to_string()))
            .messages(Vec::<Message>::new())
            .tools(tools.to_vec());
    if let Some(system_prompt) = system_prompt {
        request = request.preamble(system_prompt.to_string());
    }

    serde_json::to_vec(&request)
        .map(|json| json.len().saturating_sub(1))
        .unwrap_or(0)
}

/// Token-counting counterpart to [`completion_request_overhead`], used only by
/// the soft compaction trigger.
pub(crate) fn completion_request_overhead_tokens(
    prompt: &str,
    system_prompt: Option<&str>,
    tools: &[ToolDefinition],
) -> usize {
    let mut request =
        rig_core::completion::CompletionRequest::new(Message::user(prompt.to_string()))
            .messages(Vec::<Message>::new())
            .tools(tools.to_vec());
    if let Some(system_prompt) = system_prompt {
        request = request.preamble(system_prompt.to_string());
    }

    let json = serde_json::to_string(&request).unwrap_or_default();
    token_count(&json).saturating_sub(token_count("[]"))
}

/// Local token estimate for a serialized message array.
pub(crate) fn token_len(messages: &[Message]) -> usize {
    let Ok(json) = serde_json::to_string(messages) else {
        return usize::MAX;
    };
    token_count(&json)
}

/// Serialized byte-length of `messages` as a JSON array.
pub(crate) fn serialized_len(messages: &[Message]) -> usize {
    serde_json::to_string(messages)
        .map(|json| json.len())
        .unwrap_or(usize::MAX)
}

/// Serialized byte-length of a single [`Message`].
pub(crate) fn serialized_message_len(message: &Message) -> usize {
    serde_json::to_string(message)
        .map(|json| json.len())
        .unwrap_or(usize::MAX)
}

/// Tokenize `text` with the same `o200k_base` encoder used for landscape
/// budgets.
fn token_count(text: &str) -> usize {
    static BPE: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
    let Ok(bpe) = BPE
        .get_or_init(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string()))
        .as_ref()
    else {
        return usize::MAX;
    };
    bpe.encode_with_special_tokens(text).len()
}
