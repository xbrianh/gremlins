//! Model configuration op handlers: model, chat_model, compact.

use serde_json::Value;

use crate::clients::client::Client;
use crate::config;

use super::super::session::SessionHandle;
use super::super::{error_response, get_run_map, ok_response};

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
        if let Err(e) = Client::parse(&resolved) {
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
pub(crate) async fn handle_chat_model(_request: &Value, session: &SessionHandle) -> Value {
    let (id, parked_model) = {
        let s = session.lock().await;

        let id = s
            .chat_gremlins
            .keys()
            .next()
            .cloned()
            .or_else(|| s.current_turn.as_ref().map(|turn| turn.id.clone()));

        let parked_model = id
            .as_deref()
            .and_then(|id| s.chat_gremlins.get(id))
            .map(|gremlin| gremlin.state.read_str("model"))
            .filter(|model| !model.is_empty());

        (id, parked_model)
    };

    if let Some(model) = parked_model {
        return ok_response(serde_json::json!({ "model": model }));
    }

    let Some(id) = id else {
        return ok_response(serde_json::json!({ "model": Value::Null }));
    };

    let model = get_run_map().lock().unwrap().get(&id).and_then(|handle| {
        let model = handle.store.read_str("model");
        (!model.is_empty()).then_some(model)
    });

    match model {
        Some(model) => ok_response(serde_json::json!({ "model": model })),
        None => ok_response(serde_json::json!({ "model": Value::Null })),
    }
}

/// Handle the `/compact` TUI command.
pub(crate) async fn handle_compact(_request: &Value, session: &SessionHandle) -> Value {
    let (client, verbatim_history) = {
        let mut s = session.lock().await;
        let id = match s.chat_ids.iter().next() {
            Some(id) => id.clone(),
            None => return error_response("no active chat session to compact"),
        };
        if s.current_turn.as_ref().is_some_and(|cur| cur.id == *id) {
            return error_response("cannot compact while a chat turn is active");
        }
        let Some(gremlin) = s.chat_gremlins.get_mut(&id) else {
            return error_response("active chat gremlin not found");
        };
        let chat = gremlin
            .definition
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::definition::ChatDefinition>())
            .expect("chat gremlin must carry a ChatDefinition");
        if chat.interrupted() {
            return error_response("cannot compact an interrupted chat turn");
        }
        let client = gremlin.client.clone();
        let verbatim_history =
            crate::executor::run_agent::read_chat_conversation(gremlin, None).await;
        (client, verbatim_history)
    };

    let backend = match client.get_or_build_backend() {
        Ok(b) => b,
        Err(e) => {
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
            {
                let s = session.lock().await;
                let id = match s.chat_ids.iter().next() {
                    Some(id) => id,
                    None => return error_response("chat session vanished"),
                };
                let Some(gremlin) = s.chat_gremlins.get(id) else {
                    return error_response("active chat gremlin vanished");
                };
                if let Err(e) =
                    crate::executor::run_agent::persist_compacted_conversation(gremlin, &messages)
                        .await
                {
                    return error_response(&format!("failed to persist compacted history: {e}"));
                }
            }
            ok_response(serde_json::to_value(&report).unwrap_or_default())
        }
        None => {
            ok_response(serde_json::json!({"compacted": false, "reason": "nothing to compact"}))
        }
    }
}
