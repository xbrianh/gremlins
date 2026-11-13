//! Single-instance executor supervisor.
//! The supervisor owns the run map and the accept loop. It holds a
//! `HashMap<String, RunHandle>` and dispatches socket requests to the
//! appropriate handler.

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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::executor::event::EventKind;
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket::{self, GremlinsDaemonLock};
use crate::executor::state;

// ── Re-exports needed by external executor modules ──────────────────────
// `CancelToken` and `InteractiveHandle` are also used in mod.rs itself via
// the struct definitions below (CurrentTurn, RunHandle, DebugDetachGuard).
pub(crate) use crate::clients::cancel::CancelToken;
pub(crate) use crate::clients::interactive::InteractiveHandle;

#[allow(unused_imports)]
pub(crate) use crate::clients::interactive::{InteractiveChannels, InteractiveCommand};

// Items consumed by mod.rs itself.
pub(crate) use self::chat::{add_usage_stats, base_process_env, usage_stats_from_state};
pub(crate) use self::debug::handle_debug;
pub(crate) use self::event::{
    check_shutdown, emit_event, init_bus, init_connection_counter, subscribe_all, subscribe_run,
    ConnectionGuard,
};
pub(crate) use self::lifecycle::{launch_child, stop_child, LaunchResult, LogRelay};
pub(crate) use self::log_handler::handle_log;
pub(crate) use self::ops::resolve_store_for_id;
pub(crate) use self::ops::{
    handle_artifact, handle_create_session, handle_info, handle_launch, handle_ls, handle_model,
    handle_resume, handle_rm, handle_snapshot, handle_status, handle_stop,
    stop_session_chat_gremlin,
};
pub(crate) use self::session::{session_map, teardown_session, SessionHandle};

// Items needed by tests (referenced as `super::TypeName` in tests.rs).
#[allow(unused_imports)]
pub(crate) use self::chat::{
    chat_payload, cleanup_chat_gremlin, create_chat_gremlin, handle_chat, reinsert_chat_gremlin,
    resolve_chat_env, reusable_chat_gremlin_id,
};
#[allow(unused_imports)]
pub(crate) use self::ops::resolve_filepath_source_values;
#[allow(unused_imports)]
pub(crate) use self::ops::{handle_chat_model, handle_compact, parallel_runtime_keys};
#[allow(unused_imports)]
pub(crate) use self::session::{handle_session_main, ClientSession};

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
    /// Sender paired with `done`. Kept here so paths that clear
    /// `current_turn` outside of `run_chat_turn` (e.g. hard-cancel of an
    /// already-interrupted turn) can still publish completion.
    pub(crate) done_tx: Arc<watch::Sender<bool>>,
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
    /// Broadcast sender for stage-aware live stream subscribers (Op::StageStream with follow:true).
    pub stage_stream_broadcast: broadcast::Sender<String>,
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
    /// Held so [`RunHandle::finish`] and stop paths can unregister it,
    /// preventing the run's log task, blocking writer, and state store
    /// from living until daemon exit.
    pub log_sub: Option<crate::magic_bus::Subscriber<EventKind>>,
}

impl RunHandle {
    /// Post-run cleanup: remove from run_map, broadcast terminal state,
    /// emit daemon event, optionally signal shutdown when the daemon is idle.
    pub(crate) fn finish(
        id: &str,
        result: i32,
        state_tx: &watch::Sender<RunState>,
        shutdown_tx: Option<&watch::Sender<bool>>,
        log_sub: Option<crate::magic_bus::Subscriber<EventKind>>,
    ) {
        let run_map = get_run_map();
        let is_empty = {
            let mut map = run_map.lock().unwrap();
            // Remove before publishing — if the entry is already gone,
            // a concurrent handle_stop / stop_child won the race and
            // already published the terminal state.
            if map.remove(id).is_none() {
                return;
            }
            map.is_empty()
        };

        // Publish terminal state after winning the removal race.
        // state_tx is a clone held by the spawned task, so it outlives
        // the RunHandle that was just dropped by map.remove().
        let _ = state_tx.send(RunState {
            id: id.to_string(),
            status: if result == 0 { "done" } else { "stopped" }.to_string(),
            stage: String::new(),
            started_at: String::new(),
        });

        // Emit the terminal daemon event.
        if result == 0 {
            emit_event(EventKind::RunCompleted {
                id: id.to_string(),
                exit_code: 0,
            });
        } else {
            emit_event(EventKind::RunFailed {
                id: id.to_string(),
                exit_code: result,
                error: None,
            });
        }
        if is_empty {
            if let Some(tx) = shutdown_tx {
                check_shutdown(tx);
            }
        }

        // Unsubscribe the log-writer subscription so its receiver observes
        // Closed, the spawned log task drains and exits, and the blocking
        // writer / state store are released before daemon shutdown.
        if let Some(ref sub) = log_sub {
            init_bus().unsubscribe(sub);
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunState {
    pub id: String,
    pub status: String,
    pub stage: String,
    pub started_at: String,
}

// ---------------------------------------------------------------------------
// Global run_map — shared between the supervisor socket loop and the
// parallel executor so that forked children are first-class gremlins.
// ---------------------------------------------------------------------------

static RUN_MAP: OnceLock<Arc<Mutex<HashMap<String, RunHandle>>>> = OnceLock::new();

pub(crate) fn get_run_map() -> &'static Arc<Mutex<HashMap<String, RunHandle>>> {
    RUN_MAP.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

// ---------------------------------------------------------------------------
// Per-ID async mutex that serialises rm and resume for the same gremlin id,
// preventing clean(true) from nuking a workspace that a concurrent resume
// just registered in RUN_MAP.
// ---------------------------------------------------------------------------

type IdLockMap = HashMap<String, Arc<tokio::sync::Mutex<()>>>;

static ID_LOCKS: OnceLock<Mutex<IdLockMap>> = OnceLock::new();

pub(crate) fn get_id_lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let map = ID_LOCKS.get_or_init(|| Mutex::new(IdLockMap::new()));
    let mut map = map.lock().unwrap();
    map.entry(id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
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
    // Initialise the global run_map so launch_child/stop_child work.
    get_run_map();
    session_map();
    init_bus();
    init_connection_counter();
    // Capture the base environment once at daemon startup. This is the
    // snapshot chat requests without an explicit `env` field fall back to.
    let _ = base_process_env();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

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
                        let state_root = state_root.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            let _guard = ConnectionGuard::new(&shutdown_tx);
                            handle_connection(stream, state_root, shutdown_tx).await;
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
    // Stop session listeners first so no new session connections are
    // accepted, then tear each session down (which cancels chat turns and
    // reaps gremlins) before unlinking the control socket.
    let _ = shutdown_tx.send(true);
    let sessions: Vec<SessionHandle> = session_map().lock().unwrap().values().cloned().collect();
    for session in &sessions {
        teardown_session(session, &state_root).await;
    }
    socket::unlink_socket(&state_root);
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state_root: PathBuf,
    shutdown_tx: watch::Sender<bool>,
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
                    let value =
                        serde_json::to_value(dispatch.kind()).unwrap_or(serde_json::Value::Null);
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
            handle_log(&request, &state_root, reader, &writer).await;
            break;
        }

        if op == "debug" {
            let _ = event_start_tx.send(true);
            handle_debug(&request, &state_root, reader, &writer).await;
            break;
        }

        // stage_stream — delegated to `executor::stage_stream`.
        if op == "stage_stream" {
            let _ = event_start_tx.send(true);
            crate::executor::stage_stream::handle_stage_stream(
                &request,
                &state_root,
                reader,
                &writer,
            )
            .await;
            break;
        }

        dispatch_op(&op, &request, &state_root, &shutdown_tx, &writer).await;
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

    // After cleanup the run map may have drained; let shutdown observe it
    // once the connection counter is decremented by the accept loop.
    check_shutdown(&shutdown_tx);
}

// ---------------------------------------------------------------------------
// Op dispatch
// ---------------------------------------------------------------------------

pub(crate) async fn dispatch_op(
    op: &str,
    request: &Value,
    state_root: &Path,
    shutdown_tx: &watch::Sender<bool>,
    writer: &SharedWriter,
) {
    let resp = match op {
        "version" => ok_response(serde_json::json!({"version": crate::VERSION})),
        "launch" => handle_launch(request, state_root, shutdown_tx).await,
        "create_session" => handle_create_session(request, state_root, shutdown_tx).await,
        "stop" => {
            if let Some(id) = request.get("id").and_then(|v| v.as_str()) {
                stop_session_chat_gremlin(id).await;
            }
            handle_stop(request, shutdown_tx).await
        }
        "resume" => handle_resume(request, state_root, shutdown_tx).await,
        "ls" => handle_ls(request, state_root).await,
        "rm" => handle_rm(request, state_root).await,
        "status" => handle_status(request, state_root).await,
        "info" => handle_info(request, state_root).await,
        "artifact" => handle_artifact(request).await,
        "snapshot" => handle_snapshot(request, state_root).await,
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
