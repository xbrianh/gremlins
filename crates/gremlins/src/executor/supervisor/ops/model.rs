//! Model configuration op handlers: model, chat_model, compact.

use serde_json::Value;
use tokio::sync::mpsc;

use crate::clients::client::Client as LlmClient;
use crate::config;

use super::super::actor_client::Client;
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
pub(crate) async fn handle_chat_model(
    _request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
    session_id: &str,
) -> Value {
    match Client::new(actor_tx.clone())
        .session_get_chat_model(session_id)
        .await
    {
        Some(model) => ok_response(serde_json::json!({ "model": model })),
        None => ok_response(serde_json::json!({ "model": Value::Null })),
    }
}

/// Handle the `/compact` TUI command.
pub(crate) async fn handle_compact(
    _request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
    session_id: &str,
) -> Value {
    let ids = Client::new(actor_tx.clone())
        .session_get_chat_ids(session_id)
        .await;
    let Some(id) = ids.into_iter().next() else {
        return error_response("no active chat session to compact");
    };

    let turn_active = Client::new(actor_tx.clone())
        .session_get_current_turn(session_id)
        .await
        .is_some_and(|cur| cur.id == id);
    if turn_active {
        return error_response("cannot compact while a chat turn is active");
    }

    let Some(mut gremlin) = Client::new(actor_tx.clone())
        .session_take_gremlin(session_id, &id)
        .await
    else {
        return error_response("active chat gremlin not found");
    };

    let interrupted = gremlin
        .definition
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<crate::definition::ChatDefinition>())
        .expect("chat gremlin must carry a ChatDefinition")
        .interrupted();
    if interrupted {
        Client::new(actor_tx.clone())
            .session_reinsert_gremlin(session_id, &id, gremlin)
            .await;
        return error_response("cannot compact an interrupted chat turn");
    }

    let client = gremlin.client.clone();
    let verbatim_history = crate::executor::run_agent::read_chat_conversation(&gremlin, None).await;

    let backend = match client.get_or_build_backend() {
        Ok(b) => b,
        Err(e) => {
            Client::new(actor_tx.clone())
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
            return error_response(&format!("failed to build backend for compaction: {e}"));
        }
    };
    let mut conv = crate::conversation::standard::StandardConversation::new(
        "",
        false,
        Some(verbatim_history),
        Some(backend.clone()),
    );

    let system_prompt = crate::clients::agent::system_prompts::chat_system_prompt();

    match conv.compact(Some(system_prompt), None).await {
        Some(report) => {
            let messages = conv.committed_history();
            // Convert the error to a `String` inside the match arm so no
            // non-`Send` `Box<dyn Error>` is held across the reinsert await.
            let persist_err: Option<String> =
                match crate::executor::run_agent::persist_compacted_conversation(
                    &gremlin, &messages,
                )
                .await
                {
                    Ok(()) => None,
                    Err(e) => Some(e.to_string()),
                };
            Client::new(actor_tx.clone())
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
            match persist_err {
                None => ok_response(serde_json::to_value(&report).unwrap_or_default()),
                Some(msg) => error_response(&format!("failed to persist compacted history: {msg}")),
            }
        }
        None => {
            Client::new(actor_tx.clone())
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
            ok_response(serde_json::json!({"compacted": false, "reason": "nothing to compact"}))
        }
    }
}
