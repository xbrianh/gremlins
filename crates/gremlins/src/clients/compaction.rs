//! Compaction dispatch.
//!
//! [`Compaction`] holds the backend reference, model, instructions, strategy,
//! and (for local compaction) a pre-built model instance. Runtime orchestration
//! state — threshold, cooldown, applied flag, force flag — lives in
//! [`crate::conversation::compaction::CompactionState`].
//!
//! The strategy is picked at construction time:
//!
//! - **Server**: Native server-side compaction (Anthropic on-demand API).
//! - **Local**: Model-driven summarization on a pre-built model instance.
//!
//! There is no fallback chain. Server fails → error. Local fails → error.
//!
//! Local-compaction helpers (`compact_local_raw`, `compact_local_with_retry`,
//! and their truncation/validation support) live in
//! [`crate::conversation::compaction`] — this module is pure dispatch.

use std::sync::Arc;

use rig_core::completion::Message;

use crate::clients::backend::{Backend, ClientError};
use crate::clients::model::GremlinsModel;
use crate::clients::protocol::{CompactionBlock, CompactionResult};

/// The compaction strategy, picked at construction time.
#[derive(Clone)]
pub(crate) enum CompactionStrategy {
    /// Native server-side compaction (Anthropic on-demand API).
    Server,
    /// Local model-driven summarization.
    Local,
}

/// Compaction dispatch. Built once in `run.rs` and passed to
/// `StandardConversation::new`.
#[derive(Clone)]
pub(crate) struct Compaction {
    backend: Arc<dyn Backend>,
    model: String,
    instructions: Option<String>,
    strategy: CompactionStrategy,
    /// Model instance for local compaction, built once at construction.
    local_model: Option<GremlinsModel>,
}

impl Compaction {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        model: String,
        instructions: Option<String>,
        strategy: CompactionStrategy,
        local_model: Option<GremlinsModel>,
    ) -> Self {
        Self {
            backend,
            model,
            instructions,
            strategy,
            local_model,
        }
    }

    /// Run compaction on `to_compact`. Dispatches to the active strategy
    /// with no fallback chaining.
    ///
    /// - **Server**: calls `backend.compact()` (Anthropic on-demand API).
    ///   Returns `Ok(None)` when the API returns a null content block.
    /// - **Local**: runs a single-turn summarization completion on the
    ///   pre-built model with a single retry on transient failure. Returns
    ///   `Ok(None)` when the model can't be built or the summarization
    ///   call fails.
    ///
    /// `Err(...)` from either path propagates up to `render_turn`, which
    /// logs it and sets cooldown. There is no fallback.
    pub(crate) async fn compact(
        &self,
        to_compact: &[Message],
        kept: &[Message],
        system_prompt: Option<&str>,
    ) -> Result<Option<(Option<CompactionBlock>, Vec<Message>, CompactionResult)>, ClientError>
    {
        match self.strategy {
            CompactionStrategy::Server => {
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

                let signed: CompactionBlock = match server_block.block.clone() {
                    Some(b) => b,
                    // Server returned a compaction result with no block —
                    // unexpected but not fatal; return Ok(None) so the
                    // caller sets cooldown rather than panicking.
                    None => {
                        log::warn!("Server compaction result had no block; dropping");
                        return Ok(None);
                    }
                };
                let mut new_history = Vec::with_capacity(1 + kept.len());
                new_history.push(compaction_block_as_message(&signed));
                new_history.extend_from_slice(kept);
                Ok(Some((Some(signed), new_history, server_block)))
            }
            CompactionStrategy::Local => {
                let model = match &self.local_model {
                    Some(m) => m,
                    None => {
                        log::warn!("Compaction: local model not available");
                        return Ok(None);
                    }
                };

                let (summary_msg, input_tokens, output_tokens) =
                    match crate::conversation::compaction::compact_local_with_retry(
                        model,
                        to_compact,
                        self.instructions.as_deref(),
                        system_prompt,
                    )
                    .await
                    {
                        Some(v) => v,
                        None => return Ok(None),
                    };

                let mut new_history = Vec::with_capacity(1 + kept.len());
                new_history.push(summary_msg);
                new_history.extend_from_slice(kept);
                let result = CompactionResult {
                    block: None,
                    input_tokens,
                    output_tokens,
                };
                Ok(Some((None, new_history, result)))
            }
        }
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
