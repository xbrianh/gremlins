//! Compaction dispatch and [`compaction_block_as_message`].
//!
//! [`Compaction`] holds the backend, model, instructions, and optional
//! local model. Its [`compact`](Compaction::compact) method dispatches
//! to server-side or local compaction based on
//! [`Backend::supports_compaction`] — no fallback chain, no strategy enum.
//! The caller receives a uniform `(replacement_message, signed_block, usage)`
//! result and applies it through a single code path.
//!
//! Runtime orchestration state — threshold, cooldown, applied flag,
//! force flag — lives in [`crate::conversation::compaction::CompactionState`].
//! Local-compaction helpers live in [`crate::conversation::compaction`].

use std::sync::Arc;

use rig_core::completion::Message;

use crate::clients::backend::Backend;
use crate::clients::model::GremlinsModel;
use crate::clients::protocol::{CompactionBlock, CompactionResult};

/// Compaction dispatch. Built once in `run.rs` and passed to
/// `StandardConversation::new`.
#[derive(Clone)]
pub(crate) struct Compaction {
    backend: Arc<dyn Backend>,
    model: String,
    instructions: Option<String>,
    /// Pre-built model for local compaction. `None` when unavailable;
    /// `compact()` skips local compaction in that case.
    local_model: Option<GremlinsModel>,
}

impl Compaction {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        model: String,
        instructions: Option<String>,
        local_model: Option<GremlinsModel>,
    ) -> Self {
        Self {
            backend,
            model,
            instructions,
            local_model,
        }
    }

    /// Run compaction on `to_compact`.
    ///
    /// Dispatches to server-side or local compaction based on
    /// [`Backend::supports_compaction`]. Each path succeeds or returns
    /// `None` independently — there is no fallback chaining.
    ///
    /// Returns `(replacement_message, signed_block, usage)` on success.
    /// `signed_block` is `Some` for server compaction, `None` for local.
    /// `usage` carries token counts for logging.
    ///
    /// Returns `None` when compaction produces nothing useful. Errors
    /// are logged internally.
    pub(crate) async fn compact(
        &self,
        to_compact: &[Message],
        system_prompt: Option<&str>,
    ) -> Option<(Message, Option<CompactionBlock>, CompactionResult)> {
        if self.backend.supports_compaction() {
            self.compact_server(to_compact, system_prompt).await
        } else if let Some(model) = &self.local_model {
            self.compact_local(model, to_compact, system_prompt).await
        } else {
            log::warn!(
                "Compaction: backend lacks server support and no local model; {} messages not compacted",
                to_compact.len()
            );
            None
        }
    }

    async fn compact_server(
        &self,
        to_compact: &[Message],
        system_prompt: Option<&str>,
    ) -> Option<(Message, Option<CompactionBlock>, CompactionResult)> {
        match self
            .backend
            .compact(
                to_compact,
                system_prompt,
                self.instructions.as_deref(),
                &self.model,
            )
            .await
        {
            Ok(Some(result)) => {
                let signed = result.block.clone()?;
                let msg = compaction_block_as_message(&signed);
                Some((msg, Some(signed), result))
            }
            Ok(None) => {
                log::error!(
                    "Server compaction returned Ok(None) despite supports_compaction; \
                     {} messages not compacted",
                    to_compact.len()
                );
                None
            }
            Err(e) => {
                log::error!(
                    "Server compaction failed: {e}; {} messages not compacted",
                    to_compact.len()
                );
                None
            }
        }
    }

    async fn compact_local(
        &self,
        model: &GremlinsModel,
        to_compact: &[Message],
        system_prompt: Option<&str>,
    ) -> Option<(Message, Option<CompactionBlock>, CompactionResult)> {
        let (summary_msg, input_tokens, output_tokens) =
            crate::conversation::compaction::compact_local_with_retry(
                model,
                to_compact,
                self.instructions.as_deref(),
                system_prompt,
            )
            .await?;

        Some((
            summary_msg,
            None,
            CompactionResult {
                block: None,
                input_tokens,
                output_tokens,
            },
        ))
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
