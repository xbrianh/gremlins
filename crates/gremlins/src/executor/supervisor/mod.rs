//! Single-instance executor supervisor.
//!
//! The supervisor owns a lock-protected run registry (run map, connection
//! counter, id locks) and a per-session task registry. The accept loop
//! dispatches socket requests to the appropriate handlers, which reach the
//! registry directly instead of through an actor mailbox.

#[cfg(any(test, feature = "artifact-test-support"))]
mod artifact_test_support;
#[cfg(any(test, feature = "artifact-test-support"))]
#[doc(hidden)]
pub use artifact_test_support::ArtifactTestSupervisor;

pub(crate) mod attachment;
pub(crate) mod chat;
pub(crate) mod chat_handle;
pub(crate) mod chat_system_prompt;
pub(crate) mod debug;
pub(crate) mod lifecycle;
pub(crate) mod log_handler;
pub(crate) mod ops;
pub(crate) mod pending_scope;
pub(crate) mod prompt_msg;
pub(crate) mod run_registry;
pub(crate) mod session;
pub(crate) mod session_task;
pub(crate) mod state_view;

#[cfg(test)]
mod test_provider;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::watch;

use crate::executor::socket::{self, GremlinsDaemonLock};
use gremlins_magic_bus::connection::SharedWriter;
use gremlins_magic_bus::{init_bus, subscribe_all};

use self::run_registry::RunRegistry;
use crate::executor::gremlin::validate_gremlin_id;

// ── Re-exports needed by external executor modules ──────────────────────
#[allow(unused_imports)]
pub(crate) use crate::clients::cancel::CancelToken;

/// Registry mapping `session_id → session_task::SessionTaskHandle`.
/// Shared between the supervisor accept loop and the control-socket dispatch
/// so `stop_session_chat_gremlin` can locate owning sessions without the actor.
pub(crate) type SessionRegistry = Arc<Mutex<HashMap<String, session_task::SessionTaskHandle>>>;

// Items consumed by mod.rs itself.
pub(crate) use self::chat::base_process_env;
pub(crate) use self::lifecycle::LogRelay;
pub(crate) use self::log_handler::handle_log;
pub(crate) use self::ops::{
    handle_artifact, handle_create_session, handle_info, handle_launch, handle_ls, handle_model,
    handle_resume, handle_rm, handle_snapshot, handle_status, handle_stop,
    stop_session_chat_gremlin,
};
pub(crate) use self::run_registry::LaunchResult;
#[allow(unused_imports)]
pub(crate) use self::session::handle_session_main;

// Items needed by tests (referenced as `super::TypeName` in tests.rs).
#[allow(unused_imports)]
pub(crate) use self::chat::{
    chat_payload, create_chat_gremlin, handle_chat, resolve_chat_env, reusable_chat_gremlin_id,
};
#[allow(unused_imports)]
pub(crate) use self::ops::resolve_filepath_source_values;
#[allow(unused_imports)]
pub(crate) use self::ops::{handle_chat_model, handle_compact, parallel_runtime_keys};

/// The in-flight chat turn for a connection, if any.
#[allow(unused_imports)]
pub(crate) use self::chat::CurrentTurn;

// RunState lives in `run_registry`.
pub(crate) use self::run_registry::RunState;

/// Validate `"id"` field from a JSON-RPC-style request against [`validate_gremlin_id`].
pub(crate) fn valid_id(request: &Value) -> Result<&str, Value> {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return Err(error_response("missing 'id' field"));
    }
    if validate_gremlin_id(id).is_err() {
        return Err(error_response(&format!("invalid gremlin id {id:?}")));
    }
    Ok(id)
}

/// Run the accept loop. The `_lock` file holds the executor's exclusive advisory
/// flock for the lifetime of the supervisor.
pub async fn run_supervisor(
    listener: UnixListener,
    state_root: PathBuf,
    _lock: GremlinsDaemonLock,
) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let registry = Arc::new(RunRegistry::new(shutdown_tx));
    accept_loop(listener, state_root, registry, shutdown_rx).await;
}

async fn accept_loop(
    listener: UnixListener,
    state_root: PathBuf,
    registry: Arc<RunRegistry>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    init_bus();
    // Capture the base environment once at daemon startup. This is the
    // snapshot chat requests without an explicit `env` field fall back to.
    let _ = base_process_env();
    let sessions: SessionRegistry = Arc::new(Mutex::new(HashMap::new()));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        log::info!("supervisor: accepted connection");
                        let registry = Arc::clone(&registry);
                        let sessions = Arc::clone(&sessions);
                        tokio::spawn(async move {
                            let _guard = {
                                let registry = Arc::clone(&registry);
                                gremlins_magic_bus::connection::ConnectionGuard::new(
                                    {
                                        let registry = Arc::clone(&registry);
                                        move || registry.connection_up()
                                    },
                                    move || registry.connection_down(),
                                )
                            };
                            handle_connection(stream, registry, sessions).await;
                        });
                    }
                    Err(e) => {
                        log::error!("supervisor: accept error: {e}");
                        break;
                    }
                }
            }
        }
    }

    log::info!("supervisor: shutting down");

    // Tear down every session before unlinking the supervisor socket so each
    // session's chat gremlins are stopped, aux sockets/listener are aborted,
    // and session socket files are unlinked.
    let handles: Vec<session_task::SessionTaskHandle> =
        sessions.lock().unwrap().values().cloned().collect();
    for handle in handles {
        handle.teardown(true).await;
    }

    socket::unlink_socket(&state_root);
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    registry: Arc<RunRegistry>,
    sessions: SessionRegistry,
) {
    let (read_half, write_half) = stream.into_split();
    let writer = SharedWriter::new(write_half);
    let mut reader = BufReader::new(read_half);
    log::debug!("supervisor: handle_connection started, entering read loop");

    // Subscribe to the event broadcast *before* spawning so that no
    // events are lost between connection acceptance and task scheduling.
    let (event_sub, mut event_rx) = subscribe_all();

    // Gate the event-forwarding subtask so that one-shot clients always
    // receive their request response before any broadcast events.
    let (events_enabled_tx, mut events_enabled_rx) = watch::channel(false);
    let event_writer = writer.clone();
    let event_handle = tokio::spawn(async move {
        loop {
            let enabled = *events_enabled_rx.borrow_and_update();
            let event = tokio::select! {
                biased;
                changed = events_enabled_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    continue;
                }
                event = event_rx.recv(), if enabled => event,
            };
            match event {
                gremlins_magic_bus::Recv::Event(dispatch) => {
                    let kind = dispatch.kind();
                    // Stream events belong to per-run/session subscribers
                    // (chat, debug). Forwarding them through the global
                    // control socket duplicates chat output and lets
                    // unrelated runs' stream events leak into other sessions.
                    if kind.is_stream_event() {
                        continue;
                    }
                    let value = serde_json::to_value(kind).unwrap_or(serde_json::Value::Null);
                    if event_writer.write_json_line(&value).await.is_err() {
                        break;
                    }
                }
                gremlins_magic_bus::Recv::Lagged(skipped) => {
                    let payload = serde_json::json!({
                        "type": "event_lagged",
                        "skipped": skipped,
                    });
                    if event_writer.write_json_line(&payload).await.is_err() {
                        break;
                    }
                }
                gremlins_magic_bus::Recv::Closed => break,
            }
        }
    });

    let log_request = loop {
        let request = match socket::read_json_line(&mut reader).await {
            Ok(Some(v)) => v,
            Ok(None) => break None,
            Err(e) => {
                log::warn!("supervisor: read error: {e}");
                break None;
            }
        };

        let op = request
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        log::debug!("supervisor: received op={op:?}");

        if op == "log" {
            // The run-scoped log handler takes over after global forwarding stops.
            break Some((request, reader));
        }

        if op == "attach" {
            // Stop the global event-forwarding task before handing
            // off to the scoped attachment handler.  If a prior
            // request already opened the event gate, unrelated
            // runs' broadcasts can interleave with the per-run
            // attachment protocol.  Unsubscribe, drop the gate,
            // and await the forwarder's completion — same
            // teardown as the log handoff below.
            drop(events_enabled_tx);
            event_handle.abort();
            init_bus().unsubscribe(&event_sub);
            let _ = event_handle.await;
            attachment::handle_attach(&request, &registry, reader, &writer).await;
            // The loop-scope teardown was already performed above;
            // return directly so we don't try to abort the already-
            // consumed event handle again.
            return;
        }

        dispatch_op(&op, &request, &registry, &writer, &sessions).await;
        // Unblock events *after* the response is written so one-shot
        // clients always read the response before any broadcast event.
        // Skip for "version" — the version handshake is a one-shot
        // probe before the caller's actual request; unblocking here
        // would let a run event arrive before the real response.
        if op != "version" {
            let _ = events_enabled_tx.send(true);
        }
    };

    // On log handoff, finish the current frame but never start a queued one.
    // On disconnect there is no next writer, so don't wait for a blocked write.
    drop(events_enabled_tx);
    if log_request.is_none() {
        event_handle.abort();
    }
    init_bus().unsubscribe(&event_sub);
    let _ = event_handle.await;

    if let Some((request, reader)) = log_request {
        handle_log(&request, &registry, reader, &writer).await;
    }

    // After this connection drops the `ConnectionGuard` will fire a
    // `connection_down`, which triggers idle-shutdown inside the registry.
}

// ---------------------------------------------------------------------------
// Op dispatch
// ---------------------------------------------------------------------------

pub(crate) async fn dispatch_op(
    op: &str,
    request: &Value,
    registry: &Arc<RunRegistry>,
    writer: &SharedWriter,
    sessions: &SessionRegistry,
) {
    let resp = match op {
        "version" => ok_response(serde_json::json!({"version": crate::VERSION})),
        "launch" => handle_launch(request, registry).await,
        "create_session" => handle_create_session(request, registry, sessions).await,
        "stop" => {
            if let Some(id) = request.get("id").and_then(|v| v.as_str()) {
                stop_session_chat_gremlin(id, registry, sessions).await;
            }
            handle_stop(request, registry).await
        }
        "resume" => handle_resume(request, registry).await,
        "ls" => handle_ls(request, registry).await,
        "rm" => handle_rm(request, registry).await,
        "status" => handle_status(request, registry).await,
        "info" => handle_info(request, registry).await,
        "artifact" => handle_artifact(request, registry).await,
        "snapshot" => handle_snapshot(request, registry).await,
        "model" => handle_model(request).await,
        _ => error_response(&format!("unknown op: {op:?}")),
    };
    let _ = writer.write_json_line(&resp).await;
}

pub(crate) fn ok_response(data: Value) -> Value {
    let mut map = data.as_object().cloned().unwrap_or_default();
    map.insert("type".to_string(), Value::String("ok".to_string()));
    Value::Object(map)
}

pub(crate) fn error_response(message: &str) -> Value {
    serde_json::json!({"type": "error", "message": message})
}
