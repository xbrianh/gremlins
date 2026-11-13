//! Op handlers for the control and session sockets.
//! Re-exports from focused submodules.

pub(crate) mod control;
pub(crate) mod model;
pub(crate) mod query;

// Control ops
pub(crate) use control::{
    handle_launch, handle_resume, handle_rm, handle_session_stop, handle_stop,
    parallel_runtime_keys, resolve_filepath_source_values, stop_session_chat_gremlin,
};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::config;
use crate::executor::socket;

use super::actor::ActorMsg;
use super::session::run_session_listener;
use super::{error_response, ok_response};

// Create session stays here — it's the coordinating factory, not purely
// control or query.
pub(crate) async fn handle_create_session(
    _request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
) -> Value {
    // Session ids use the same charset as gremlin ids but a distinct prefix
    // so a session id can never collide with a gremlin id. The actor allocates
    // and registers the session so a concurrent create cannot race.
    let (respond, rx) = oneshot::channel();
    if actor_tx.send(ActorMsg::CreateSession { respond }).is_err() {
        return error_response("supervisor is shutting down");
    }
    let session_id = match rx.await {
        Ok(session_id) => session_id,
        Err(_) => return error_response("supervisor is shutting down"),
    };

    let state_root = config::state_root();
    let listener = match socket::bind_session_socket(&state_root, &session_id) {
        Ok(listener) => listener,
        Err(e) => {
            // Roll back the registration so a failed bind doesn't leak a
            // permanently-unusable session.
            let (respond, _rx) = oneshot::channel();
            let _ = actor_tx.send(ActorMsg::RemoveSession {
                session_id: session_id.clone(),
                respond,
            });
            return error_response(&e);
        }
    };

    let listener_actor_tx = actor_tx.clone();
    let listener_session_id = session_id.clone();
    let listener_task = tokio::spawn(async move {
        run_session_listener(listener, listener_session_id, listener_actor_tx).await;
    });

    // Stash the listener handle so teardown/supervisor shutdown can abort it
    // and stop accepting new connections.
    let (respond, _rx) = oneshot::channel();
    let _ = actor_tx.send(ActorMsg::SessionSetListenerTask {
        session_id: session_id.clone(),
        task: listener_task,
        respond,
    });

    ok_response(serde_json::json!({
        "session_id": session_id,
        "socket": format!("{session_id}.sock"),
    }))
}

// Query ops
pub(crate) use query::{
    handle_artifact, handle_info, handle_ls, handle_snapshot, handle_status, resolve_store_for_id,
};

// Model ops
pub(crate) use model::{handle_chat_model, handle_compact, handle_model};
