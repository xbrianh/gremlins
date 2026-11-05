//! Server-side conversation compaction via the Anthropic compact API beta.
//!
//! [`CompactionConfig`] holds the backend reference, model, threshold, and
//! instructions needed to trigger compaction. Its [`compact`](CompactionConfig::compact)
//! method calls the backend and wraps the result.

use std::sync::Arc;

use rig_core::completion::Message;

use crate::clients::backend::{Backend, ClientError};
use crate::clients::protocol::CompactionBlock;

/// Compaction strategy and execution. Built once in `run.rs` and passed
/// to `StandardConversation::new`. Holds an `Arc<dyn Backend>` so it is
/// cheap to clone.
#[derive(Clone)]
pub(crate) struct CompactionConfig {
    backend: Arc<dyn Backend>,
    model: String,
    threshold: u64,
    keep_recent: usize,
    instructions: Option<String>,
}

impl CompactionConfig {
    #[allow(dead_code)]
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
        }
    }

    pub(crate) fn threshold(&self) -> u64 {
        self.threshold
    }

    pub(crate) fn keep_recent(&self) -> usize {
        self.keep_recent
    }

    /// Run server-side compaction on `to_compact`.
    /// Returns `Ok(None)` when the backend doesn't support compaction.
    pub(crate) async fn compact(
        &self,
        to_compact: &[Message],
        kept: &[Message],
        system_prompt: Option<&str>,
    ) -> Result<Option<(CompactionBlock, Vec<Message>)>, ClientError> {
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
        let mut new_history = Vec::with_capacity(1 + kept.len());
        new_history.push(compaction_block_as_message(&server_block.block));
        new_history.extend_from_slice(kept);
        Ok(Some((server_block.block, new_history)))
    }
}

/// Wrap a [`CompactionBlock`] as an assistant [`Message`] suitable for
/// inclusion in the message history.
///
/// The block is stored as a text assistant message whose `additional_params`
/// carry the signature and other signed fields so the block survives
/// round-tripping through request serialization and transcript persistence.
pub(crate) fn compaction_block_as_message(block: &CompactionBlock) -> Message {
    use rig_core::completion::message::{AdditionalParams, AssistantContent, Text};

    let mut params = serde_json::Map::new();
    params.insert(
        "signature".into(),
        serde_json::Value::String(block.signature.clone()),
    );
    if let Some(ref ec) = block.encrypted_content {
        params.insert(
            "encrypted_content".into(),
            serde_json::Value::String(ec.clone()),
        );
    }
    if let Some(ref tc) = block.tool_changes {
        params.insert(
            "tool_changes".into(),
            serde_json::to_value(tc).unwrap_or_default(),
        );
    }
    // cache_control enables prompt caching for the compaction block.
    let mut cache = serde_json::Map::new();
    cache.insert("type".into(), serde_json::Value::String("ephemeral".into()));
    params.insert("cache_control".into(), serde_json::Value::Object(cache));
    let text = Text {
        text: format!("[compacted]: {}", block.content),
        additional_params: AdditionalParams::new(params),
    };
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(text)],
    }
}
