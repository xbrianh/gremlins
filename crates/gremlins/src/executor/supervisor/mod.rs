//! Single-instance executor supervisor.
//!
//! The supervisor owns an actor task that holds the run map, session map, and
//! connection counter. The accept loop dispatches socket requests to the
//! appropriate handlers, which communicate with the actor via
//! [`ActorMsg`] messages.

pub(crate) mod actor;
pub(crate) mod actor_client;
pub(crate) mod chat;
pub(crate) mod debug;
pub(crate) mod event;
pub(crate) mod lifecycle;
pub(crate) mod log_handler;
pub(crate) mod ops;
pub(crate) mod session;
pub(crate) mod state_view;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};

use crate::executor::event::EventKind;
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket::{self, GremlinsDaemonLock};
use crate::executor::state;

// ── Re-exports needed by external executor modules ──────────────────────
pub(crate) use crate::clients::cancel::CancelToken;
pub(crate) use crate::clients::interactive::InteractiveHandle;

#[allow(unused_imports)]
pub(crate) use crate::clients::interactive::{InteractiveChannels, InteractiveCommand};

// Items consumed by mod.rs itself.
pub(crate) use self::chat::{add_usage_stats, base_process_env, usage_stats_from_state};
pub(crate) use self::debug::handle_debug;
pub(crate) use self::event::{
    emit_event, emit_run_event_async, init_bus, subscribe_all, subscribe_run,
    subscribe_run_backpressured,
};
pub(crate) use self::lifecycle::{launch_child, stop_child, LaunchResult, LogRelay};
pub(crate) use self::log_handler::handle_log;
pub(crate) use self::ops::resolve_store_for_id;
pub(crate) use self::ops::{
    handle_artifact, handle_create_session, handle_info, handle_launch, handle_ls, handle_model,
    handle_resume, handle_rm, handle_snapshot, handle_status, handle_stop,
    stop_session_chat_gremlin,
};
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

// ---------------------------------------------------------------------------
// SharedWriter — Arc<Mutex<WriteHalf>> for concurrent socket writes
// ---------------------------------------------------------------------------

pub(crate) struct SharedWriter {
    inner: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
}

impl SharedWriter {
    pub(crate) fn new(inner: tokio::net::unix::OwnedWriteHalf) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
        }
    }

    pub(crate) async fn write_json_line(&self, value: &Value) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        socket::write_json_line(&mut *guard, value).await
    }
}

impl Clone for SharedWriter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// RAII guard that restores pause and sends Quit on drop (even during abort).
pub(crate) struct DebugDetachGuard {
    interactive: Option<InteractiveHandle>,
}

impl DebugDetachGuard {
    pub(crate) fn new(interactive: InteractiveHandle) -> Self {
        Self {
            interactive: Some(interactive),
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.interactive = None;
    }
}

impl Drop for DebugDetachGuard {
    fn drop(&mut self) {
        if let Some(interactive) = &self.interactive {
            let _ = interactive.cmd_tx.try_send(InteractiveCommand::Quit);
            interactive.pause.reset();
        }
    }
}

/// RAII guard that unsubscribes a bus subscriber on drop.
///
/// Chat turns and debug sessions register run-scoped subscriptions; a
/// subscription that outlived its consumer would accumulate and — once its
/// bounded buffer filled — stall the agent loop of a later run that reuses the
/// same id. Dropping this guard unsubscribes on every exit path, including
/// early returns.
pub(crate) struct BusSubGuard {
    subscriber: Option<crate::magic_bus::Subscriber<EventKind>>,
}

impl BusSubGuard {
    pub(crate) fn new(subscriber: crate::magic_bus::Subscriber<EventKind>) -> Self {
        Self {
            subscriber: Some(subscriber),
        }
    }
}

impl Drop for BusSubGuard {
    fn drop(&mut self) {
        if let Some(subscriber) = self.subscriber.take() {
            init_bus().unsubscribe(&subscriber);
        }
    }
}

/// The in-flight chat turn for a connection, if any.
pub(crate) struct CurrentTurn {
    pub(crate) id: String,
    /// Client-generated turn id, used to match a `cancel_chat` to the turn
    /// it targets so a late cancel cannot hit a subsequent turn.
    pub(crate) turn: Option<String>,
    pub(crate) cancel: Arc<CancelToken>,
    /// Set when the latest `cancel_chat` requested a soft interrupt. The
    /// forwarding task reads this when its cancel token fires to decide
    /// between recovering the partial turn and discarding it.
    pub(crate) soft_cancel: Arc<AtomicBool>,
    /// Set to `true` once the turn has re-inserted its gremlin (or dropped
    /// it). A follow-up `chat` for the same id waits on this so it never
    /// sees the gremlin as "unknown" mid-turn. Waiters clone the receiver;
    /// the value is sticky so a waiter that subscribes after the turn
    /// finished still proceeds immediately.
    pub(crate) done: watch::Receiver<bool>,
    /// True when a soft-interrupt has already emitted `ended` for this
    /// turn. The turn stays alive so a hard cancel within the double-press
    /// window can still clean up the partial artifact and interrupt flag.
    pub(crate) interrupted: bool,
}

// ---------------------------------------------------------------------------
// RunHandle
// ---------------------------------------------------------------------------

pub(crate) struct RunHandle {
    pub cancel: Arc<CancelToken>,
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub aborted: Arc<AtomicBool>,
    #[allow(dead_code)]
    pub state_tx: watch::Sender<RunState>,
    /// Interactive handle (supervisor → agent loop).
    pub interactive: InteractiveHandle,
    /// State store handle — stored so handle_status / handle_info / handle_ls
    /// can read state directly for live gremlins whose state may be in a
    /// TempDir (ephemeral runs) rather than under config::state_root().
    pub store: Arc<dyn state::StateStore + Send + Sync>,
    /// Flush barrier for the log writer: a follower that wants to read
    /// persisted history can request a flush and wait for the ack before
    /// opening the log blob, so that pre-subscription events are visible.
    pub log_flush: Option<tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>>,
    /// Bus subscriber for the run's lossless log-writer subscription.
    /// Held so finish and stop paths can unregister it, preventing the run's
    /// log task, blocking writer, and state store from living until daemon exit.
    pub log_sub: Option<crate::magic_bus::Subscriber<EventKind>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunState {
    pub id: String,
    pub status: String,
    pub stage: String,
    pub started_at: String,
}

// ---------------------------------------------------------------------------
// Connection guard — RAII counter via the actor
// ---------------------------------------------------------------------------

/// RAII guard: sends `ConnectionUp` on construction, `ConnectionDown` on drop.
pub(crate) struct ConnectionGuard {
    actor_tx: Option<mpsc::UnboundedSender<actor::ActorMsg>>,
}

impl ConnectionGuard {
    pub(crate) fn new(actor_tx: &mpsc::UnboundedSender<actor::ActorMsg>) -> Self {
        let _ = actor_tx.send(actor::ActorMsg::ConnectionUp);
        Self {
            actor_tx: Some(actor_tx.clone()),
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.actor_tx.take() {
            let _ = tx.send(actor::ActorMsg::ConnectionDown);
        }
    }
}

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
    init_bus();
    // Capture the base environment once at daemon startup. This is the
    // snapshot chat requests without an explicit `env` field fall back to.
    let _ = base_process_env();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    // Spawn the actor — it owns the run map, session map, connection counter,
    // and idle-shutdown logic.
    let actor_tx = actor::spawn_actor(state_root.clone(), shutdown_tx);

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
                        let actor_tx = actor_tx.clone();
                        tokio::spawn(async move {
                            let _guard = ConnectionGuard::new(&actor_tx);
                            handle_connection(stream, actor_tx).await;
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

    // Tear down every session. The actor does the surgery; we just fire the
    // requests and wait for completion.
    let (respond, rx) = tokio::sync::oneshot::channel();
    let _ = actor_tx.send(actor::ActorMsg::SessionGetAll { respond });
    if let Ok(session_ids) = rx.await {
        for session_id in session_ids {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let _ = actor_tx.send(actor::ActorMsg::SessionTeardown {
                session_id,
                abort_listener: true,
                respond: done_tx,
            });
            let _ = done_rx.await;
        }
    }

    socket::unlink_socket(&state_root);
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    actor_tx: mpsc::UnboundedSender<actor::ActorMsg>,
) {
    let (read_half, write_half) = stream.into_split();
    let writer = SharedWriter::new(write_half);
    let mut reader = BufReader::new(read_half);
    log::debug!("supervisor: handle_connection started, entering read loop");

    // Subscribe to the event broadcast *before* spawning so that no
    // events are lost between connection acceptance and task scheduling.
    let (_event_sub, mut event_rx) = subscribe_all();

    // Gate the event-forwarding subtask so that one-shot clients always
    // receive their request response before any broadcast events.
    let (event_start_tx, mut event_start_rx) = tokio::sync::watch::channel(false);
    let event_writer = writer.clone();
    let event_handle = tokio::spawn(async move {
        // Wait until the first request has been fully processed.
        let _ = event_start_rx.wait_for(|v| *v).await;
        loop {
            match event_rx.recv().await {
                crate::magic_bus::Recv::Event(dispatch) => {
                    let kind = dispatch.kind();
                    // Stream events belong to per-run/session subscribers
                    // (chat, debug). Forwarding them through the global
                    // control socket duplicates chat output and lets
                    // unrelated runs' stream events leak into other sessions.
                    if matches!(
                        kind,
                        EventKind::StreamChunk { .. }
                            | EventKind::ReasoningChunk { .. }
                            | EventKind::ToolOutputChunk { .. }
                            | EventKind::ToolResult { .. }
                            | EventKind::TurnComplete { .. }
                            | EventKind::Done { .. }
                            | EventKind::StreamReset
                            | EventKind::Ready { .. }
                            | EventKind::Ended { .. }
                            | EventKind::ExecOutput { .. }
                    ) {
                        continue;
                    }
                    let value = serde_json::to_value(kind).unwrap_or(serde_json::Value::Null);
                    if event_writer.write_json_line(&value).await.is_err() {
                        break;
                    }
                }
                crate::magic_bus::Recv::Lagged(skipped) => {
                    let payload = serde_json::json!({
                        "type": "event_lagged",
                        "skipped": skipped,
                    });
                    if event_writer.write_json_line(&payload).await.is_err() {
                        break;
                    }
                }
                crate::magic_bus::Recv::Closed => break,
            }
        }
    });

    loop {
        let request = match socket::read_json_line(&mut reader).await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                log::warn!("supervisor: read error: {e}");
                break;
            }
        };

        let op = request
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        log::debug!("supervisor: received op={op:?}");

        // Streaming pipeline ops take ownership of the connection; handle
        // them directly so they can monitor the read half for disconnect.
        if op == "log" {
            // Unblock events now that the streaming op is starting.
            let _ = event_start_tx.send(true);
            handle_log(&request, &actor_tx, reader, &writer).await;
            break;
        }

        if op == "debug" {
            let _ = event_start_tx.send(true);
            handle_debug(&request, &actor_tx, reader, &writer).await;
            break;
        }

        // stage_stream — delegated to `executor::stage_stream`.
        if op == "stage_stream" {
            let _ = event_start_tx.send(true);
            crate::executor::stage_stream::handle_stage_stream(
                &request, &actor_tx, reader, &writer,
            )
            .await;
            break;
        }

        dispatch_op(&op, &request, &actor_tx, &writer).await;
        // Unblock events *after* the response is written so one-shot
        // clients always read the response before any broadcast event.
        // Skip for "version" — the version handshake is a one-shot
        // probe before the caller's actual request; unblocking here
        // would let a run event arrive before the real response.
        if op != "version" {
            let _ = event_start_tx.send(true);
        }
    }

    // Abort the event-forwarding subtask so it doesn't leak the write
    // half or linger waiting for broadcasts after the client is gone.
    event_handle.abort();

    // Unregister the bus subscription so repeated control connections don't
    // accumulate permanent work for each emission.
    init_bus().unsubscribe(&_event_sub);

    // After this connection drops the `ConnectionGuard` will fire a
    // `ConnectionDown` message, which triggers idle-shutdown inside the actor.
}

// ---------------------------------------------------------------------------
// Op dispatch
// ---------------------------------------------------------------------------

pub(crate) async fn dispatch_op(
    op: &str,
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<actor::ActorMsg>,
    writer: &SharedWriter,
) {
    let resp = match op {
        "version" => ok_response(serde_json::json!({"version": crate::VERSION})),
        "launch" => handle_launch(request, actor_tx).await,
        "create_session" => handle_create_session(request, actor_tx).await,
        "stop" => {
            if let Some(id) = request.get("id").and_then(|v| v.as_str()) {
                stop_session_chat_gremlin(id, actor_tx).await;
            }
            handle_stop(request, actor_tx).await
        }
        "resume" => handle_resume(request, actor_tx).await,
        "ls" => handle_ls(request, actor_tx).await,
        "rm" => handle_rm(request, actor_tx).await,
        "status" => handle_status(request, actor_tx).await,
        "info" => handle_info(request, actor_tx).await,
        "artifact" => handle_artifact(request, actor_tx).await,
        "snapshot" => handle_snapshot(request, actor_tx).await,
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
