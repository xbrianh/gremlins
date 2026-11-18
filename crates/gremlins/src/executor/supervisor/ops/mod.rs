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
use std::sync::Arc;

use crate::config;

use crate::executor::socket;
use crate::executor::state;

use super::run_registry::RunRegistry;
use super::session::run_session_listener;
use super::session_task::{spawn_session_task, SessionTaskHandle};
use super::{error_response, ok_response, SessionRegistry};

// Create session stays here — it's the coordinating factory, not purely
// control or query.
pub(crate) async fn handle_create_session(
    _request: &Value,
    registry: &Arc<RunRegistry>,
    sessions: &SessionRegistry,
) -> Value {
    // Session ids use the same charset as gremlin ids but a distinct prefix
    // so a session id can never collide with a gremlin id. The registry lock
    // is held across both the collision check and the insert, so two
    // concurrent `create_session` calls cannot reserve the same id.
    let state_root = config::state_root();
    let registry_for_session = Arc::clone(registry);
    let (session_id, session_handle) = {
        let mut registry = sessions.lock().unwrap();
        loop {
            let candidate = format!("sess-{}", state::token_hex(8));
            if !registry.contains_key(&candidate) {
                let handle: SessionTaskHandle = spawn_session_task(
                    state_root.clone(),
                    candidate.clone(),
                    Arc::clone(&registry_for_session),
                    Some(sessions.clone()),
                );
                registry.insert(candidate.clone(), handle.clone());
                break (candidate, handle);
            }
        }
    };

    let listener = match socket::bind_session_socket(&state_root, &session_id) {
        Ok(listener) => listener,
        Err(e) => {
            // Roll the session back so a failed bind does not leak a
            // registry entry or a session task.
            session_handle.teardown(false).await;
            return error_response(&e);
        }
    };

    let listener_registry = Arc::clone(registry);
    let listener_session_id = session_id.clone();
    let listener_session_task = session_handle.clone();
    let listener_task = tokio::spawn(async move {
        run_session_listener(
            listener,
            listener_session_id,
            listener_registry,
            listener_session_task,
        )
        .await;
    });

    // Stash the listener handle with the session task so teardown can abort it.
    session_handle.set_listener_task(listener_task).await;

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
