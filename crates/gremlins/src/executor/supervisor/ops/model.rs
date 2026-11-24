//! Model configuration op handlers: model, chat_model, compact.

use serde_json::Value;

use crate::clients::client::Client as LlmClient;
use crate::clients::conversation::Conversation;
use crate::config;

use super::super::session_task::SessionTaskHandle;
use super::super::{error_response, ok_response};

/// Show, set, or clear the TUI chat model override.
pub(crate) async fn handle_model(request: &Value) -> Value {
    if let Err(e) = config::init_global() {
        return error_response(&format!("config init: {e}"));
    }

    let cfg = match config::global_config() {
        Ok(cfg) => cfg,
        Err(e) => return error_response(&format!("config error: {e}")),
    };

    let clear = request
        .get("clear")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if clear {
        if let Err(e) = config::clear_tui_model_override() {
            return error_response(&e);
        }
    } else if let Some(spec) = request.get("model").and_then(|v| v.as_str()) {
        let spec = spec.trim();
        if spec.is_empty() {
            return error_response("missing 'model' field");
        }

        let resolved = match config::resolve_model_reference(spec, &cfg) {
            Ok(resolved) => resolved,
            Err(e) => return error_response(&format!("invalid model: {e}")),
        };
        if let Err(e) = LlmClient::parse(&resolved) {
            return error_response(&format!("invalid model spec {resolved:?}: {e}"));
        }
        if let Err(e) = config::write_tui_model_override(&resolved) {
            return error_response(&e);
        }
    }

    match config::resolve_tui_chat_model(&cfg) {
        Ok(Some((source, spec))) => ok_response(serde_json::json!({
            "model": spec,
            "source": source.as_str(),
        })),
        Ok(None) => ok_response(serde_json::json!({
            "model": Value::Null,
            "source": Value::Null,
            "message": "chat model: not configured",
        })),
        Err(e) => error_response(&format!("invalid model: {e}")),
    }
}

/// Read the active chat gremlin's resolved model from `state.json`.
pub(crate) async fn handle_chat_model(_request: &Value, session_task: &SessionTaskHandle) -> Value {
    match session_task.get_chat_model().await {
        Some(model) => ok_response(serde_json::json!({ "model": model })),
        None => ok_response(serde_json::json!({ "model": Value::Null })),
    }
}

/// Handle the `/compact` TUI command.
pub(crate) async fn handle_compact(_request: &Value, session_task: &SessionTaskHandle) -> Value {
    let ids = session_task.get_chat_ids().await;
    let Some(id) = ids.into_iter().next() else {
        return error_response("no active chat session to compact");
    };

    let turn_active = session_task
        .get_current_turn()
        .await
        .is_some_and(|cur| cur.id == id);
    if turn_active {
        return error_response("cannot compact while a chat turn is active");
    }

    let Some(mut gremlin) = session_task.take_gremlin(&id).await else {
        return error_response("active chat gremlin not found");
    };

    let interrupted = gremlin
        .definition
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<crate::definition::ChatDefinition>())
        .expect("chat gremlin must carry a ChatDefinition")
        .interrupted();
    if interrupted {
        session_task.reinsert_gremlin(&id, gremlin).await;
        return error_response("cannot compact an interrupted chat turn");
    }

    let client = gremlin.client.clone();
    let (verbatim_history, request_context) =
        crate::executor::run_agent::read_chat_conversation_with_context(&gremlin, None).await;

    let backend = match client.get_or_build_backend() {
        Ok(b) => b,
        Err(e) => {
            session_task.reinsert_gremlin(&id, gremlin).await;
            return error_response(&format!("failed to build backend for compaction: {e}"));
        }
    };
    let mut conv = reconstruct_compaction_conversation(verbatim_history, request_context, backend);
    let system_prompt = conv
        .request_context()
        .and_then(|c| c.system)
        .unwrap_or_else(|| {
            crate::executor::supervisor::chat_system_prompt::chat_system_prompt().to_owned()
        });

    match conv.compact(Some(&system_prompt), None).await {
        Some(report) => {
            let messages = conv.committed_history();
            // Convert the error to a `String` inside the match arm so no
            // non-`Send` `Box<dyn Error>` is held across the reinsert await.
            let persist_err: Option<String> =
                match crate::executor::run_agent::persist_compacted_conversation_with_context(
                    &gremlin,
                    &messages,
                    conv.request_context(),
                )
                .await
                {
                    Ok(()) => None,
                    Err(e) => Some(e.to_string()),
                };
            session_task.reinsert_gremlin(&id, gremlin).await;
            match persist_err {
                None => ok_response(serde_json::to_value(&report).unwrap_or_default()),
                Some(msg) => error_response(&format!("failed to persist compacted history: {msg}")),
            }
        }
        None => {
            session_task.reinsert_gremlin(&id, gremlin).await;
            ok_response(serde_json::json!({"compacted": false, "reason": "nothing to compact"}))
        }
    }
}

/// Manual compaction restores the request that produced this history, rather
/// than resolving a new model, system prompt, or tool inventory.
fn reconstruct_compaction_conversation(
    history: Vec<rig_core::completion::Message>,
    context: Option<crate::clients::compaction::ResolvedRequestContext>,
    backend: std::sync::Arc<dyn crate::clients::backend::Backend>,
) -> crate::conversation::standard::StandardConversation {
    let mut conversation = crate::conversation::standard::StandardConversation::new(
        "",
        false,
        Some(history),
        Some(backend),
    );
    if let Some(context) = context {
        conversation.set_request_context(context);
    }
    conversation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::backend::{Backend, ClientError, RunParams};
    use crate::clients::compaction::ResolvedRequestContext;
    use crate::clients::protocol::{CompactionResult, CompletedRun};
    use crate::clients::InteractiveSession;
    use crate::conversation::compaction::CompactionStrategy;
    use rig_core::completion::{Message, ToolDefinition};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct ManualBackend(Mutex<Vec<(Vec<Message>, ResolvedRequestContext)>>);

    #[async_trait::async_trait]
    impl Backend for ManualBackend {
        async fn run(
            &self,
            _: RunParams,
            _: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            panic!("manual compaction must not run a normal request")
        }
        fn name(&self) -> &'static str {
            "anthropic"
        }
        fn model(&self) -> &str {
            "new-configured-model-not-the-original"
        }
        fn supports_compaction(&self) -> bool {
            true
        }
        async fn compact(
            &self,
            messages: &[Message],
            context: &ResolvedRequestContext,
            _: Option<&str>,
        ) -> Result<Option<CompactionResult>, ClientError> {
            self.0
                .lock()
                .unwrap()
                .push((messages.to_vec(), context.clone()));
            Ok(Some(CompactionResult {
                block: Some(
                    serde_json::from_value(
                        json!({"content": "manual summary", "signature": "signed"}),
                    )
                    .unwrap(),
                ),
                input_tokens: Some(41),
                output_tokens: Some(7),
            }))
        }
    }

    fn history() -> Vec<Message> {
        (0..20)
            .flat_map(|i| {
                [
                    Message::user(format!("question {i}")),
                    Message::assistant(format!("answer {i}")),
                ]
            })
            .collect()
    }

    fn context() -> ResolvedRequestContext {
        ResolvedRequestContext {
            model: "original-effective-model".into(),
            system: Some("original resolved system and workspace preamble".into()),
            tools: vec![ToolDefinition {
                name: rig_core::completion::message::ToolName::new("PreviouslyAvailableTool")
                    .unwrap(),
                description: "original resolved tool, not freshly enumerated".into(),
                parameters: json!({"type":"object", "properties":{"original":{"type":"string"}}}),
                annotations: Default::default(),
            }],
            extra: Some(json!({"thinking":{"type":"adaptive"}, "original":true})),
            temperature: Some(0.35),
            max_tokens: Some(2345),
        }
    }

    #[tokio::test]
    async fn manual_reconstruction_reuses_exact_request_context_for_server_compaction() {
        let backend = Arc::new(ManualBackend::default());
        let expected = serde_json::to_value(context()).unwrap();
        // Exercise the same deserialization boundary used by persisted history.
        let restored = serde_json::from_value(expected.clone()).unwrap();
        let history = history();
        let mut conv =
            reconstruct_compaction_conversation(history.clone(), Some(restored), backend.clone());
        conv.set_local_summarizer(|_, _, _, _| panic!("server context must be reused"));
        let report = conv.compact(None, None).await.unwrap();
        assert!(matches!(report.strategy, CompactionStrategy::Server));
        let calls = backend.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, history[..report.messages_compacted]);
        assert_eq!(serde_json::to_value(&calls[0].1).unwrap(), expected);
        assert_eq!(
            serde_json::to_value(conv.request_context().unwrap()).unwrap(),
            expected
        );
        assert_eq!(report.compaction_input_tokens, Some(41));
    }

    #[tokio::test]
    async fn manual_reconstruction_without_context_skips_server_and_falls_back_locally() {
        let backend = Arc::new(ManualBackend::default());
        let mut conv = reconstruct_compaction_conversation(history(), None, backend.clone());
        conv.set_local_summarizer(|_, messages, _, _| {
            assert!(!messages.is_empty());
            Box::pin(async { Some((Message::assistant("local manual summary"), None, None)) })
        });
        let report = conv.compact(None, None).await.unwrap();
        assert!(matches!(report.strategy, CompactionStrategy::Local));
        assert!(backend.0.lock().unwrap().is_empty());
        assert!(conv.request_context().is_none());
    }
}
