//! [`compaction_block_as_message`] helper — the only public item in this module
//! now that dispatch has been inlined into [`StandardConversation`].
//!
//! Compaction orchestration state lives in
//! [`crate::conversation::compaction::CompactionState`].
//! Local-compaction helpers live in [`crate::conversation::compaction`].

use rig_core::completion::message::{Api, Origin};
use rig_core::completion::Message;

use crate::clients::protocol::CompactionBlock;

/// The Anthropic wire's replay identity, as
/// [`ReplayTarget::api`](rig_core::completion::ReplayTarget::api) and
/// [`provider`](rig_core::completion::ReplayTarget::provider) report it.
const ANTHROPIC_API: Api = Api::from_static("anthropic.messages");
const ANTHROPIC_PROVIDER: &str = "anthropic";

/// Wrap a [`CompactionBlock`] as an assistant [`Message`] suitable for
/// inclusion in the message history.
///
/// The block is stored as an assistant message whose `Text` content carries
/// the full compaction block as its native provider item, so rig's Anthropic
/// wire code resends the raw `type: "compaction"` content block during
/// serialization. The `text` field holds the unmodified signed content for
/// signature preservation.
///
/// # Why the origin matters
///
/// In rig-core 0.46 a native item is only replayed when the turn it sits on
/// [`Origin`]s to the model the request addresses: `adapt` strips provider
/// items from a turn whose origin is absent or names another model. A native
/// item on an origin-less turn is therefore dropped silently — the compaction
/// block would never reach the wire, and the model would receive a summary it
/// never signed. `model` is the model the backend is configured for, which is
/// the one both the compaction call and every subsequent turn address.
///
/// The `Text` block, not [`Opaque`](rig_core::completion::message::Opaque),
/// carries the item on purpose: when the origin stops matching — the model was
/// switched under a resumed transcript — a `Text` block degrades to its
/// canonical text, so the summary survives as ordinary assistant text. An
/// `Opaque` block would instead delete the whole message, losing the summary.
pub(crate) fn compaction_block_as_message(block: &CompactionBlock, model: &str) -> Message {
    use rig_core::completion::message::{AssistantContent, AssistantMessage, Text};

    // Build the full Anthropic wire content block. This is attached as the
    // Text block's native provider item so the Anthropic request encoder
    // resends it verbatim instead of the canonical text projection.
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

    // The text is the signed summary itself: it is what the turn falls back
    // to if the native item stops replaying.
    let text = Text::new(block.content.clone());
    let origin = Origin::new(ANTHROPIC_API, ANTHROPIC_PROVIDER, model);
    Message::Assistant(
        AssistantMessage::new(vec![
            AssistantContent::Text(text).with_native(serde_json::Value::Object(raw_block))
        ])
        .with_origin(origin),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{AssistantContent, ToolName};
    use rig_core::completion::{CompletionRequest, ToolDefinition};
    use rig_core::operation::Completion;
    use rig_core::wire::{Body, Mode, Operation, Wire};

    const MODEL: &str = "claude-sonnet-4-6";

    fn block() -> CompactionBlock {
        CompactionBlock {
            content: "SUMMARY".into(),
            signature: "SIG".into(),
            encrypted_content: None,
            tool_changes: None,
        }
    }

    fn user() -> Message {
        Message::User {
            content: vec![rig_core::completion::message::UserContent::text("hi")],
        }
    }

    /// The Anthropic request body for `history` addressed to `model`.
    ///
    /// Goes through `prepare` then `encode`, exactly as the driver does, so
    /// `adapt`'s replay gate is exercised rather than bypassed.
    fn body(history: Vec<Message>, model: &str) -> serde_json::Value {
        let wire = rig_core::providers::anthropic::Anthropic::new("k").completion(model);
        let mut request =
            CompletionRequest::new("go")
                .preamble("sys")
                .tools(vec![ToolDefinition {
                    name: ToolName::new("Do").unwrap(),
                    description: "d".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                    annotations: Default::default(),
                }]);
        request.chat_history = history.into_iter().map(std::sync::Arc::new).collect();
        let request = <Completion as Operation>::prepare(request, &wire.wire.describe())
            .expect("request prepares");
        let encoded = wire.wire.encode(request, Mode::Streaming).expect("encodes");
        match encoded.request.body() {
            Body::Bytes(bytes) => serde_json::from_slice(bytes).expect("body is JSON"),
            Body::Multipart(_) => panic!("Anthropic requests are not multipart"),
        }
    }

    fn assistant_blocks(body: &serde_json::Value) -> &Vec<serde_json::Value> {
        body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("an assistant turn")["content"]
            .as_array()
            .expect("content")
    }

    /// The regression this guards: a native item on an origin-less turn is
    /// stripped by `adapt`, so the compaction block never reaches the wire.
    #[test]
    fn compaction_block_reaches_the_wire() {
        let messages = body(
            vec![user(), compaction_block_as_message(&block(), MODEL)],
            MODEL,
        );
        assert_eq!(
            assistant_blocks(&messages),
            &vec![serde_json::json!({
                "type": "compaction",
                "content": "SUMMARY",
                "signature": "SIG",
                "cache_control": {"type": "ephemeral"},
            })],
            "the signed compaction block must be resent verbatim"
        );
    }

    /// Without the origin, `adapt` drops the item and the turn degrades to
    /// plain text — the failure this fix removes.
    #[test]
    fn without_the_origin_the_block_is_lost() {
        let Message::Assistant(mut turn) = compaction_block_as_message(&block(), MODEL) else {
            panic!("assistant turn");
        };
        turn.origin = None;
        let messages = body(vec![user(), Message::Assistant(turn)], MODEL);
        assert_eq!(
            assistant_blocks(&messages),
            &vec![serde_json::json!({"type": "text", "text": "SUMMARY"})],
            "an origin-less turn loses its native item"
        );
    }

    /// When the model no longer matches the origin, the block cannot be
    /// replayed — but the summary must survive as text rather than vanish.
    #[test]
    fn a_switched_model_keeps_the_summary_text() {
        let history = vec![user(), compaction_block_as_message(&block(), MODEL)];
        let messages = body(history, "claude-opus-4-6");
        assert_eq!(
            assistant_blocks(&messages),
            &vec![serde_json::json!({"type": "text", "text": "SUMMARY"})],
            "the summary degrades to text instead of disappearing"
        );
    }

    /// The turn carries no stray canonical text beside the replayed block.
    #[test]
    fn the_replayed_turn_has_one_block() {
        let Message::Assistant(turn) = compaction_block_as_message(&block(), MODEL) else {
            panic!("assistant turn");
        };
        assert_eq!(turn.content.len(), 1);
        assert!(matches!(turn.content[0], AssistantContent::Text(_)));
        assert!(turn.origin.is_some());
    }
}
