//! Shared resolved request inputs and signed-compaction replay helpers.
//!
//! Compaction orchestration state lives in
//! [`crate::conversation::compaction::CompactionState`].
//! Local-compaction helpers live in [`crate::conversation::compaction`].

use rig_core::completion::message::{Api, Origin};
use rig_core::completion::{CompletionRequest, Message, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Ephemeral request ownership; never persisted in provider request context.
#[derive(Clone, Debug)]
pub(crate) struct UsageAttribution {
    pub corr_id: gremlins_magic_bus::CorrId,
    pub agent_id: String,
}

/// The resolved inputs shared by normal completion and server compaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedRequestContext {
    /// Empty for older artifacts; unknown providers cannot use server compaction.
    #[serde(default)]
    pub(crate) provider: String,
    #[serde(skip)]
    pub(crate) usage_attribution: Option<UsageAttribution>,
    pub(crate) model: String,
    pub(crate) system: Option<String>,
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) extra: Option<Value>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
}

impl ResolvedRequestContext {
    pub(crate) fn request(&self, messages: Vec<Message>) -> CompletionRequest {
        let mut request = CompletionRequest::from(messages).tools(self.tools.clone());
        if let Some(system) = &self.system {
            request = request.preamble(system.clone());
        }
        if let Some(extra) = &self.extra {
            request = request.additional_params(extra.clone());
        }
        if let Some(temperature) = self.temperature {
            request = request.temperature(temperature);
        }
        if let Some(max_tokens) = self.max_tokens {
            request = request.max_tokens(max_tokens);
        }
        request
    }
}

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

    use crate::clients::backend::{Backend, RunParams};
    use crate::clients::conversation::Conversation;
    use crate::clients::error::ClientError;
    use crate::clients::interactive::InteractiveSession;
    use crate::clients::protocol::{CompactionResult, CompletedRun};
    use crate::conversation::standard::StandardConversation;
    use rig_core::completion::message::{
        AssistantMessage, Opaque, Reasoning, Text, ToolCall, ToolFunction,
    };
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    const MODEL: &str = "claude-sonnet-4-6";
    const NEXT: &str = "DISTINCT NEXT PROMPT";

    struct CompactionBackend {
        provider: &'static str,
        received: Mutex<Vec<Message>>,
        server_summary: bool,
    }

    #[async_trait::async_trait]
    impl Backend for CompactionBackend {
        async fn run(
            &self,
            _: RunParams,
            _: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            panic!("offline compaction tests must not run a model")
        }
        fn name(&self) -> &'static str {
            self.provider
        }
        fn model(&self) -> &str {
            "configured-model-not-effective-model"
        }
        fn supports_compaction(&self) -> bool {
            true
        }
        async fn compact(
            &self,
            messages: &[Message],
            context: &ResolvedRequestContext,
            instructions: Option<&str>,
        ) -> Result<Option<CompactionResult>, ClientError> {
            assert_eq!(context.system.as_deref(), Some("system"));
            assert_eq!(context.model, MODEL);
            assert_eq!(
                serde_json::to_value(context).unwrap(),
                serde_json::to_value(crate::clients::compaction::tests::context()).unwrap()
            );
            assert_eq!(instructions, Some("instructions"));
            *self.received.lock().unwrap() = messages.to_vec();
            if !self.server_summary {
                return Ok(None);
            }
            Ok(Some(CompactionResult {
                block: Some(full_block()),
                input_tokens: Some(123),
                output_tokens: Some(45),
            }))
        }
    }

    fn backend(provider: &'static str) -> Arc<CompactionBackend> {
        Arc::new(CompactionBackend {
            provider,
            received: Mutex::new(vec![]),
            server_summary: true,
        })
    }

    #[test]
    fn usage_attribution_is_ephemeral() {
        let mut request = context();
        request.usage_attribution = Some(UsageAttribution {
            corr_id: gremlins_magic_bus::CorrId::new().with_run("live-run"),
            agent_id: "parent/child".into(),
        });
        let persisted = serde_json::to_value(&request).unwrap();
        assert!(persisted.get("usage_attribution").is_none());
        let restored: ResolvedRequestContext = serde_json::from_value(persisted).unwrap();
        assert!(restored.usage_attribution.is_none());
    }

    fn context() -> ResolvedRequestContext {
        ResolvedRequestContext {
            usage_attribution: None,
            provider: "anthropic".into(),
            model: MODEL.into(),
            system: Some("system".into()),
            tools: vec![ToolDefinition {
                name: ToolName::new("Do").unwrap(),
                description: "resolved tool".into(),
                parameters: json!({"type": "object", "properties": {}}),
                annotations: Default::default(),
            }],
            extra: Some(json!({"thinking": {"type": "adaptive"}})),
            temperature: Some(0.7),
            max_tokens: Some(4096),
        }
    }

    #[test]
    fn resolved_request_uses_exact_history_and_roundtrips() {
        let context: ResolvedRequestContext =
            serde_json::from_value(serde_json::to_value(context()).unwrap()).unwrap();
        for messages in [vec![], older_prefix(), retained()] {
            let request = context.request(messages.clone());
            assert_eq!(
                request
                    .chat_history
                    .iter()
                    .map(|m| m.as_ref().clone())
                    .collect::<Vec<_>>(),
                std::iter::once(Message::system("system"))
                    .chain(messages)
                    .collect::<Vec<_>>()
            );
            let json = serde_json::to_value(request).unwrap();
            assert_eq!(json["tools"], serde_json::to_value(&context.tools).unwrap());
            assert_eq!(json["additional_params"], context.extra.clone().unwrap());
            assert_eq!(json["temperature"], 0.7);
            assert_eq!(json["max_tokens"], 4096);
        }
    }

    fn full_block() -> CompactionBlock {
        CompactionBlock {
            encrypted_content: Some("ENCRYPTED SUMMARY".into()),
            tool_changes: Some(vec![json!({"name": "Do", "change": "preserved"})]),
            ..block()
        }
    }

    fn assistant(content: Vec<AssistantContent>) -> Message {
        Message::Assistant(AssistantMessage::new(content).with_origin(Origin::new(
            ANTHROPIC_API,
            ANTHROPIC_PROVIDER,
            MODEL,
        )))
    }

    fn thinking() -> Value {
        json!({"type": "thinking", "thinking": "SIGNED REASONING", "signature": "THINKING SIG"})
    }

    fn retained() -> Vec<Message> {
        let call = ToolCall::from_wire(
            "call-1",
            ToolFunction::new(ToolName::new("Do").unwrap(), json!({})),
        );
        let mut messages = vec![
            Message::user("RETAINED USER"),
            assistant(vec![
                AssistantContent::Reasoning(Reasoning::new("REASONING WITHOUT NATIVE")),
                AssistantContent::Reasoning(Reasoning::new("SIGNED REASONING"))
                    .with_native(thinking()),
                AssistantContent::Text(Text::new("redacted projection"))
                    .with_native(json!({"type": "redacted_thinking", "data": "SEALED"})),
                AssistantContent::Opaque(Opaque {
                    item: thinking(),
                    replay: true,
                }),
                AssistantContent::Opaque(Opaque {
                    item: json!({"type": "redacted_thinking", "data": "OPAQUE SEALED"}),
                    replay: true,
                }),
                AssistantContent::Text(Text::new("thinking projection")).with_native(thinking()),
                AssistantContent::Text(Text::new("RETAINED TEXT"))
                    .with_native(json!({"type": "text", "text": "RETAINED TEXT"})),
                AssistantContent::ToolCall(call.clone()).with_native(
                    json!({"type": "tool_use", "id": "call-1", "name": "Do", "input": {}}),
                ),
            ]),
            Message::tool_result(call.id, call.function.name, "TOOL RESULT"),
            assistant(vec![AssistantContent::Reasoning(Reasoning::new(
                "THINKING ONLY",
            ))
            .with_native(thinking())]),
        ];
        for i in 0..9 {
            messages.extend([
                Message::user(format!("KEPT {i}")),
                Message::assistant(format!("ANSWER {i}")),
            ]);
        }
        messages
    }

    fn older_prefix() -> Vec<Message> {
        vec![Message::user("OLD USER"), Message::assistant("OLD ANSWER")]
    }

    fn artifact_roundtrip(history: Vec<Message>) -> Vec<Message> {
        serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap()
    }

    fn expected_tail() -> Vec<Value> {
        let mut expected = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "RETAINED USER"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "RETAINED TEXT"},
                {"type": "tool_use", "id": "call-1", "name": "Do", "input": {}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": [{"type": "text", "text": "TOOL RESULT"}]}]}),
        ];
        // Removing the thinking-only turn makes these adjacent users; the wire merges them.
        expected[2]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "text", "text": "KEPT 0"}));
        expected
            .push(json!({"role": "assistant", "content": [{"type": "text", "text": "ANSWER 0"}]}));
        for i in 1..9 {
            expected.extend([
                json!({"role": "user", "content": [{"type": "text", "text": format!("KEPT {i}")}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": format!("ANSWER {i}")}]}),
            ]);
        }
        expected.push(json!({"role": "user", "content": [{"type": "text", "text": NEXT}]}));
        expected
    }

    fn assert_compacted_wire(conv: &StandardConversation, summary: Value) {
        let committed = conv.committed_history();
        assert_eq!(
            committed.len(),
            22,
            "summary plus retained user/tool exchange; thinking-only turn removed"
        );
        let Message::Assistant(turn) = &committed[2] else {
            panic!("retained mixed assistant turn must survive");
        };
        assert_eq!(
            turn.content.len(),
            2,
            "only visible text and tool call survive"
        );
        assert!(
            turn.content
                .iter()
                .all(|block| !matches!(block, AssistantContent::Reasoning(_))),
            "reasoning without native data must be removed before wire encoding"
        );
        let restored = artifact_roundtrip(committed.clone());
        assert_eq!(restored, committed);
        let encoded = body_with_prompt(restored, MODEL, NEXT);
        let mut expected = vec![json!({"role": "assistant", "content": [summary]})];
        expected.extend(expected_tail());
        assert_eq!(encoded["messages"], json!(expected));
    }

    #[tokio::test]
    async fn server_compaction_commits_sanitized_tail_and_replays_full_signed_summary() {
        let backend = backend("anthropic");
        let prefix = older_prefix();
        let mut history = prefix.clone();
        history.extend(retained());
        let mut conv = StandardConversation::new(NEXT, true, Some(history), Some(backend.clone()));
        conv.set_request_context(context());
        let report = conv
            .compact(Some("system"), Some("instructions"))
            .await
            .expect("server success");
        assert!(matches!(
            report.strategy,
            crate::conversation::compaction::CompactionStrategy::Server
        ));
        let cut = prefix.len();
        assert_eq!(*backend.received.lock().unwrap(), prefix[..cut]);
        assert_eq!(report.messages_compacted, cut);
        assert_eq!(report.messages_kept, retained().len());
        assert_eq!(report.compaction_input_tokens, Some(123));
        assert_eq!(report.compaction_output_tokens, Some(45));
        assert_compacted_wire(
            &conv,
            json!({
                "type": "compaction", "content": "SUMMARY", "signature": "SIG",
                "encrypted_content": "ENCRYPTED SUMMARY",
                "tool_changes": [{"name": "Do", "change": "preserved"}],
                "cache_control": {"type": "ephemeral"}
            }),
        );
    }

    #[test]
    fn children_inherit_backend_until_explicitly_selected() {
        let parent = StandardConversation::new(NEXT, true, None, Some(backend("anthropic")));
        for mut child in [parent.fork(), parent.spawn_child("child prompt")] {
            assert_eq!(child.request_provider(), Some("anthropic"));
            assert!(child.request_context().is_none());
            let history = child.messages();
            child.set_request_context(context());
            child.set_request_backend(Some(backend("openai")));
            assert_eq!(child.request_provider(), Some("openai"));
            assert!(child.request_context().is_none());
            assert_eq!(child.messages(), history);
            assert_eq!(parent.request_provider(), Some("anthropic"));

            // Reselecting even the same provider must invalidate resolved inputs.
            child.set_request_context(context());
            child.set_request_backend(Some(backend("openai")));
            assert!(child.request_context().is_none());
            child.set_request_context(context());
            child.set_request_backend(None);
            assert_eq!(child.request_provider(), None);
            assert!(child.request_context().is_none());
        }
    }

    #[tokio::test]
    async fn selected_backend_requires_matching_nonempty_context_for_server_compaction() {
        // Include two empty providers: equality alone must not authorize server compaction.
        for (selected_provider, context_provider, server) in [
            ("anthropic", "anthropic", true),
            ("openai", "anthropic", false),
            ("anthropic", "openai", false),
            ("anthropic", "", false),
            ("", "", false),
        ] {
            let original = backend("original");
            let selected = backend(selected_provider);
            let mut history = older_prefix();
            history.extend(retained());
            let mut conv =
                StandardConversation::new(NEXT, true, Some(history), Some(original.clone()));
            conv.set_request_context(context());
            conv.set_request_backend(Some(selected.clone()));
            assert!(conv.request_context().is_none());
            assert_eq!(conv.request_provider(), Some(selected_provider));
            let mut resolved = context();
            resolved.provider = context_provider.into();
            conv.set_request_context(resolved);
            conv.set_local_summarizer(|spec, messages, _, _| {
                assert!(spec.ends_with("/configured-model-not-effective-model"));
                assert_eq!(messages, older_prefix());
                Box::pin(async { Some((Message::assistant("LOCAL SUMMARY"), None, None)) })
            });
            let report = conv
                .compact(Some("system"), Some("instructions"))
                .await
                .expect("server or local summary");
            assert_eq!(
                matches!(
                    report.strategy,
                    crate::conversation::compaction::CompactionStrategy::Server
                ),
                server,
                "selected={selected_provider:?}, context={context_provider:?}",
            );
            assert_eq!(!selected.received.lock().unwrap().is_empty(), server);
            assert!(original.received.lock().unwrap().is_empty());
            assert_eq!(report.compaction_block.is_some(), server);
        }
    }

    #[tokio::test]
    async fn missing_context_skips_server_and_uses_local_fallback() {
        let backend = backend("anthropic");
        let mut history = older_prefix();
        history.extend(retained());
        let mut conv = StandardConversation::new(NEXT, true, Some(history), Some(backend.clone()));
        conv.set_local_summarizer(|_, _, _, _| {
            Box::pin(async { Some((Message::assistant("LOCAL SUMMARY"), None, None)) })
        });
        let report = conv.compact(None, None).await.expect("local fallback");
        assert!(matches!(
            report.strategy,
            crate::conversation::compaction::CompactionStrategy::Local
        ));
        assert!(backend.received.lock().unwrap().is_empty());
        assert!(conv.request_context().is_none());
    }

    #[tokio::test]
    async fn local_compaction_commits_plain_summary_and_sanitized_tail() {
        let backend = Arc::new(CompactionBackend {
            provider: "anthropic",
            received: Mutex::new(vec![]),
            server_summary: false,
        });
        let prefix = older_prefix();
        let mut history = prefix.clone();
        history.extend(retained());
        let mut conv = StandardConversation::new(NEXT, true, Some(history), Some(backend.clone()));
        conv.set_local_summarizer(|spec, messages, instructions, system| {
            assert_eq!(spec, "anthropic/configured-model-not-effective-model");
            assert_eq!(messages, older_prefix());
            assert_eq!(instructions, Some("instructions"));
            assert_eq!(system, Some("system"));
            Box::pin(async { Some((Message::assistant("LOCAL SUMMARY"), Some(67), Some(8))) })
        });
        conv.set_request_context(context());
        let report = conv
            .compact(Some("system"), Some("instructions"))
            .await
            .expect("local success after server returns no block");
        assert!(matches!(
            report.strategy,
            crate::conversation::compaction::CompactionStrategy::Local
        ));
        assert_eq!(*backend.received.lock().unwrap(), prefix);
        assert_eq!(report.messages_compacted, prefix.len());
        assert_eq!(report.messages_kept, retained().len());
        assert_eq!(report.compaction_input_tokens, Some(67));
        assert_eq!(report.compaction_output_tokens, Some(8));
        assert!(report.compaction_block.is_none());
        assert_compacted_wire(&conv, json!({"type": "text", "text": "LOCAL SUMMARY"}));
    }

    #[test]
    fn uncompacted_history_still_replays_signed_and_opaque_thinking() {
        let kept = retained();
        let conv =
            StandardConversation::new(NEXT, true, Some(kept.clone()), Some(backend("anthropic")));
        assert_eq!(conv.committed_history(), kept);
        let mut signed_control = artifact_roundtrip(conv.committed_history());
        // This control isolates signed replay; plain reasoning is checked in committed history.
        if let Message::Assistant(turn) = &mut signed_control[1] {
            turn.content.remove(0);
        }
        let encoded = body_with_prompt(signed_control, MODEL, NEXT);
        assert_eq!(
            encoded["messages"][1]["content"],
            json!([
                thinking(), {"type": "redacted_thinking", "data": "SEALED"}, thinking(),
                {"type": "redacted_thinking", "data": "OPAQUE SEALED"}, thinking(),
                {"type": "text", "text": "RETAINED TEXT"},
                {"type": "tool_use", "id": "call-1", "name": "Do", "input": {}}
            ])
        );
        assert_eq!(encoded["messages"][3]["content"], json!([thinking()]));
    }

    #[tokio::test]
    async fn other_provider_compaction_sanitizes_anthropic_origin_before_switching_back() {
        let backend = Arc::new(CompactionBackend {
            provider: "openai",
            received: Mutex::new(vec![]),
            server_summary: false,
        });
        let mut history = older_prefix();
        history.extend(retained());
        let mut conv = StandardConversation::new(NEXT, true, Some(history), Some(backend));
        conv.set_local_summarizer(|spec, messages, _, _| {
            assert_eq!(spec, "openai/configured-model-not-effective-model");
            assert_eq!(messages, older_prefix());
            Box::pin(async { Some((Message::assistant("OTHER SUMMARY"), None, None)) })
        });
        conv.compact(Some("system"), Some("instructions"))
            .await
            .expect("other provider local compaction succeeds");
        // The harness replays persisted history on the original Anthropic model.
        assert_compacted_wire(&conv, json!({"type": "text", "text": "OTHER SUMMARY"}));
    }

    #[test]
    fn compaction_respects_non_anthropic_origins_and_unstamped_fallback() {
        for provider in ["openai", "anthropic"] {
            let mut kept = retained();
            for message in &mut kept {
                if let Message::Assistant(turn) = message {
                    turn.origin = Some(Origin::new(
                        Api::from_static("openai.responses"),
                        "openai",
                        "other-model",
                    ));
                }
            }
            kept.push(Message::Assistant(AssistantMessage::new(vec![
                AssistantContent::Reasoning(Reasoning::new("UNSTAMPED REASONING")),
            ])));
            let mut conv = StandardConversation::new(NEXT, true, None, Some(backend(provider)));
            let summary = Message::assistant("OTHER SUMMARY");
            conv.commit_compaction(summary.clone(), &kept);
            if provider == "anthropic" {
                kept.pop();
            }
            let mut expected = vec![summary];
            expected.extend(kept);
            assert_eq!(artifact_roundtrip(conv.committed_history()), expected);
        }
    }

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
        body_with_prompt(history, model, "go")
    }

    fn body_with_prompt(history: Vec<Message>, model: &str, prompt: &str) -> Value {
        let wire = rig_core::providers::anthropic::Anthropic::new("k").completion(model);
        let mut request =
            CompletionRequest::new(prompt)
                .preamble("sys")
                .tools(vec![ToolDefinition {
                    name: ToolName::new("Do").unwrap(),
                    description: "d".into(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                    annotations: Default::default(),
                }]);
        request.chat_history = history
            .into_iter()
            .chain([Message::user(prompt)])
            .map(std::sync::Arc::new)
            .collect();
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
