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

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::watch;

use crate::executor::socket;
use crate::executor::state;

use super::session::{run_session_listener, session_map, ClientSession};
use super::{error_response, ok_response};

// Create session stays here — it's the coordinating factory, not purely
// control or query.
pub(crate) async fn handle_create_session(
    _request: &Value,
    state_root: &Path,
    shutdown_tx: &watch::Sender<bool>,
) -> Value {
    // Session ids use the same charset as gremlin ids but a distinct prefix
    // so a session id can never collide with a gremlin id.
    let session_id = loop {
        let candidate = format!("sess-{}", state::token_hex(8));
        let mut map = session_map().lock().unwrap();
        if !map.contains_key(&candidate) {
            map.insert(
                candidate.clone(),
                Arc::new(tokio::sync::Mutex::new(ClientSession::new(
                    candidate.clone(),
                ))),
            );
            break candidate;
        }
    };

    let listener = match socket::bind_session_socket(state_root, &session_id) {
        Ok(listener) => listener,
        Err(e) => {
            // Roll back the map insertion so a failed bind doesn't leak a
            // permanently-unusable session.
            session_map().lock().unwrap().remove(&session_id);
            return error_response(&e);
        }
    };

    let session = session_map()
        .lock()
        .unwrap()
        .get(&session_id)
        .cloned()
        .expect("session is registered");
    let shutdown_tx = shutdown_tx.clone();
    let state_root = state_root.to_path_buf();
    let session_id_for_listener = session_id.clone();
    let listener_session = Arc::clone(&session);
    let listener_task = tokio::spawn(async move {
        run_session_listener(
            listener,
            session_id_for_listener,
            listener_session,
            state_root,
            shutdown_tx,
        )
        .await;
    });

    // Stash the listener handle so teardown/supervisor shutdown can abort it
    // and stop accepting new connections.
    session.lock().await.listener_task = Some(listener_task);

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
