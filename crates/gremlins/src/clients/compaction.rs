//! Server-side conversation compaction via the Anthropic compact API beta.
//!
//! [`Compaction`] holds the backend reference, model, threshold, instructions,
//! and the runtime compaction state (signed block, applied flag, cooldown).
//! Its [`compact`](Compaction::compact) method calls the backend and wraps
//! the result.

use std::sync::Arc;

use rig_core::completion::message::UserContent;
use rig_core::completion::Message;

use crate::clients::backend::{Backend, ClientError};
use crate::clients::protocol::{CompactionBlock, CompactionResult};

/// Compute a cut index so `to_compact = &history[..cut]` and
/// `kept = &history[cut..]`. The cut lands on a completed-turn boundary:
/// `keep_recent` whole user/assistant exchanges are kept verbatim and
/// `to_compact` never ends in an unanswered tool call.
pub(crate) fn compaction_cut_index(history: &[Message], keep_recent: usize) -> usize {
    let mut cut = history.len();
    let mut kept_turns = 0;
    while cut > 0 && kept_turns < keep_recent {
        // Skip trailing user tool_result messages belonging to the
        // preceding turn.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
        // Step back over this turn's user opener and its assistant/tool
        // messages to the previous completed-turn boundary.
        cut = previous_turn_boundary(history, cut);
        kept_turns += 1;
    }
    cut
}

/// Walk backward from `cut` (exclusive) to the start of the preceding
/// completed user+assistant turn, skipping assistant/tool messages.
fn previous_turn_boundary(history: &[Message], mut cut: usize) -> usize {
    // Step back over a chain of (assistant, user tool_result) pairs until
    // we hit a user message that is NOT a tool result (the turn opener).
    loop {
        // Skip the assistant message.
        if cut > 0 && matches!(history[cut - 1], Message::Assistant { .. }) {
            cut -= 1;
        } else {
            break;
        }
        // Skip any user tool_result messages after the assistant.
        while cut > 0 && is_user_tool_result(&history[cut - 1]) {
            cut -= 1;
        }
    }
    // Step back over the user opener.
    if cut > 0 && matches!(history[cut - 1], Message::User { .. }) {
        cut -= 1;
    }
    cut
}

/// True when the message is a user message containing exclusively tool
/// results (no mixed text + tool-result blocks).
fn is_user_tool_result(msg: &Message) -> bool {
    match msg {
        Message::User { content } => {
            !content.is_empty()
                && content
                    .iter()
                    .all(|c| matches!(c, UserContent::ToolResult(_)))
        }
        _ => false,
    }
}

/// Compaction strategy, execution, and runtime state. Built once in
/// `run.rs` and passed to `StandardConversation::new`.
///
/// Cloning creates a snapshot of the runtime state (`block`, `applied`,
/// `cooldown`) — forked conversations inherit but do not share
/// compaction state.
#[derive(Clone)]
pub(crate) struct Compaction {
    backend: Arc<dyn Backend>,
    model: String,
    threshold: u64,
    keep_recent: usize,
    instructions: Option<String>,
    /// Active signed compaction block, if any has been applied.
    block: Option<CompactionBlock>,
    /// True once any summary has been swapped into history.
    applied: bool,
    /// Set after a compaction call fails or produces no summary. Prevents
    /// retrying the expensive API call on every subsequent turn.
    cooldown: bool,
}

impl Compaction {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        model: String,
        threshold: u64,
        keep_recent: usize,
        instructions: Option<String>,
    ) -> Self {
        Self {
            backend,
            model,
            threshold,
            keep_recent,
            instructions,
            block: None,
            applied: false,
            cooldown: false,
        }
    }

    pub(crate) fn keep_recent(&self) -> usize {
        self.keep_recent
    }

    // ── runtime state accessors ──────────────────────────────

    pub(crate) fn block(&self) -> Option<&CompactionBlock> {
        self.block.as_ref()
    }

    /// Record that a compaction block has been applied to history.
    /// Sets `applied` and stores the block atomically, preserving
    /// the invariant that `applied` is `true` iff a block is set.
    pub(crate) fn mark_applied(&mut self, block: CompactionBlock) {
        self.block = Some(block);
        self.applied = true;
    }

    pub(crate) fn set_cooldown(&mut self) {
        self.cooldown = true;
    }

    /// Decide whether to trigger compaction on this turn.
    pub(crate) fn should_compact(
        &self,
        last_input_tokens: Option<u64>,
        history: &[Message],
    ) -> bool {
        if self.cooldown {
            return false;
        }
        let tokens = last_input_tokens.unwrap_or(0);
        if tokens <= self.threshold {
            return false;
        }
        // After compaction has been applied, don't re-compact until there is
        // genuinely new material after the block.
        if self.applied {
            let cut = compaction_cut_index(history, self.keep_recent);
            if cut == 0 {
                return false;
            }
        }
        true
    }

    /// Run server-side compaction on `to_compact`.
    /// Returns `Ok(None)` when the backend doesn't support compaction.
    pub(crate) async fn compact(
        &self,
        to_compact: &[Message],
        kept: &[Message],
        system_prompt: Option<&str>,
    ) -> Result<Option<(CompactionBlock, Vec<Message>, CompactionResult)>, ClientError> {
        let result = self
            .backend
            .compact(
                to_compact,
                system_prompt,
                self.instructions.as_deref(),
                &self.model,
            )
            .await?;
        let server_block = match result {
            Some(r) => r,
            None => return Ok(None),
        };
        let signed = server_block.block.clone();
        let mut new_history = Vec::with_capacity(1 + kept.len());
        new_history.push(compaction_block_as_message(&signed));
        new_history.extend_from_slice(kept);
        Ok(Some((signed, new_history, server_block)))
    }
}

/// Wrap a [`CompactionBlock`] as an assistant [`Message`] suitable for
/// inclusion in the message history.
///
/// The block is stored as an assistant message whose `additional_params`
/// carry the full compaction block under the `anthropic_content` key, so rig's
/// Anthropic wire code replaces the text placeholder with the raw `type:
/// "compaction"` content block during serialization. The `text` field holds
/// the unmodified signed content for signature preservation.
///
/// The `anthropic_content` key is the canonical rig-core mechanism
/// (`ANTHROPIC_RAW_CONTENT_KEY`) for round-tripping content blocks that have
/// no direct `AssistantContent` variant (server_tool_use, web_search_tool_result,
/// code_execution_tool_result, and compaction blocks).
pub(crate) fn compaction_block_as_message(block: &CompactionBlock) -> Message {
    use rig_core::completion::message::{AdditionalParams, AssistantContent, Text};

    // Build the full Anthropic wire content block. This is placed under
    // `anthropic_content` in additional_params so the Anthropic request
    // encoder substitutes it verbatim for the Text content block.
    let mut raw_block = serde_json::Map::new();
    raw_block.insert(
        "type".into(),
        serde_json::Value::String("compaction".into()),
    );
    raw_block.insert(
        "content".into(),
        serde_json::Value::String(block.content.clone()),
    );
    raw_block.insert(
        "signature".into(),
        serde_json::Value::String(block.signature.clone()),
    );
    if let Some(ref ec) = block.encrypted_content {
        raw_block.insert(
            "encrypted_content".into(),
            serde_json::Value::String(ec.clone()),
        );
    }
    if let Some(ref tc) = block.tool_changes {
        raw_block.insert(
            "tool_changes".into(),
            serde_json::to_value(tc).unwrap_or_default(),
        );
    }
    // cache_control enables prompt caching for the compaction block.
    let mut cache = serde_json::Map::new();
    cache.insert("type".into(), serde_json::Value::String("ephemeral".into()));
    raw_block.insert("cache_control".into(), serde_json::Value::Object(cache));

    let mut params = serde_json::Map::new();
    params.insert(
        "anthropic_content".into(),
        serde_json::Value::Object(raw_block),
    );
    let text = Text {
        // Store the unmodified signed content. The text field is a
        // placeholder — rig replaces it with the `anthropic_content`
        // block during serialization.
        text: block.content.clone(),
        additional_params: AdditionalParams::new(params),
    };
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(text)],
    }
}
