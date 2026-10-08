//! Single-instance executor supervisor.
//!
//! The supervisor owns the run map and the accept loop. It holds a
//! `HashMap<String, RunHandle>` and dispatches socket requests to the
//! appropriate handler.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::clients::agent_loop::CancelToken;
use crate::clients::client::Client;
use crate::clients::interactive::{
    InteractiveChannels, InteractiveCommand, InteractiveEvent, InteractiveHandle,
};
use crate::config;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin, GremlinConfig};
use crate::executor::socket::{self, GremlinsDaemonLock};
use crate::executor::state;

// ---------------------------------------------------------------------------
// SharedWriter — Arc<Mutex<WriteHalf>> for concurrent socket writes
// ---------------------------------------------------------------------------

struct SharedWriter {
    inner: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
}

impl SharedWriter {
    fn new(inner: tokio::net::unix::OwnedWriteHalf) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
        }
    }

    async fn write_json_line(&self, value: &Value) -> Result<(), String> {
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

// ---------------------------------------------------------------------------
// DaemonEvent — broadcast to subscribers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
pub enum DaemonEvent {
    #[serde(rename = "run_started")]
    RunStarted {
        id: String,
        definition: String,
        stage: String,
    },
    #[serde(rename = "run_completed")]
    RunCompleted { id: String, exit_code: i32 },
    #[serde(rename = "run_failed")]
    RunFailed {
        id: String,
        exit_code: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "run_stopped")]
    RunStopped { id: String },
    // Forward-looking variants — not wired up yet.
    #[serde(rename = "stage_transition")]
    StageTransition { id: String, stage: String },
    #[serde(rename = "log_line")]
    LogLine { id: String, line: String },
    #[serde(rename = "bail")]
    Bail { id: String, reason: String },
}

// ---------------------------------------------------------------------------
// Global event broadcast channel
// ---------------------------------------------------------------------------

static EVENT_BROADCAST: OnceLock<broadcast::Sender<DaemonEvent>> = OnceLock::new();

fn init_event_broadcast() -> &'static broadcast::Sender<DaemonEvent> {
    EVENT_BROADCAST.get_or_init(|| {
        let (tx, _) = broadcast::channel(256);
        tx
    })
}

fn get_event_tx() -> &'static broadcast::Sender<DaemonEvent> {
    init_event_broadcast()
}

fn emit_event(event: DaemonEvent) {
    if let Some(tx) = EVENT_BROADCAST.get() {
        let _ = tx.send(event);
    }
}

// ---------------------------------------------------------------------------
// Global connection counter
// ---------------------------------------------------------------------------

static CONNECTION_COUNT: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

fn init_connection_counter() -> &'static Arc<AtomicUsize> {
    CONNECTION_COUNT.get_or_init(|| Arc::new(AtomicUsize::new(0)))
}

fn get_connection_count() -> &'static AtomicUsize {
    init_connection_counter().as_ref()
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
    /// Broadcast sender for live log subscribers (Op::Log with follow:true).
    pub log_broadcast: broadcast::Sender<String>,
    /// Interactive handle (supervisor → agent loop).
    pub interactive: InteractiveHandle,
    /// State store handle — stored so handle_status / handle_info / handle_ls
    /// can read state directly for live gremlins whose state may be in a
    /// TempDir (ephemeral runs) rather than under config::state_root().
    pub store: Arc<dyn state::StateStore + Send + Sync>,
    /// Scratch directory path for live gremlins.
    pub scratch_dir: PathBuf,
}

impl RunHandle {
    /// Post-run cleanup: remove from run_map, broadcast terminal state,
    /// emit daemon event, optionally signal shutdown when the daemon is idle.
    fn finish(
        id: &str,
        result: i32,
        state_tx: &watch::Sender<RunState>,
        shutdown_tx: Option<&watch::Sender<bool>>,
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
            emit_event(DaemonEvent::RunCompleted {
                id: id.to_string(),
                exit_code: 0,
            });
        } else {
            emit_event(DaemonEvent::RunFailed {
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

/// The `_lock` file holds the executor's exclusive advisory flock for the
/// lifetime of the supervisor. When the process exits (after the last
/// gremlin drains and the last connection closes), the file descriptor
/// closes and the kernel releases the lock automatically.
pub async fn run_supervisor(
    listener: UnixListener,
    state_root: PathBuf,
    _lock: GremlinsDaemonLock,
) {
    // Initialise the global run_map so launch_child/stop_child work.
    get_run_map();
    init_event_broadcast();
    init_connection_counter();
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
                        get_connection_count().fetch_add(1, Ordering::Relaxed);
                        let state_root = state_root.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        let shutdown_tx2 = shutdown_tx.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, state_root, shutdown_tx).await;
                            let prev = get_connection_count().fetch_sub(1, Ordering::Relaxed);
                            if prev == 1 {
                                check_shutdown(&shutdown_tx2);
                            }
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
    let mut event_rx = get_event_tx().subscribe();

    // Gate the event-forwarding subtask so that one-shot clients always
    // receive their request response before any broadcast events.
    let event_start = Arc::new(tokio::sync::Notify::new());
    let event_start_clone = Arc::clone(&event_start);
    let event_writer = writer.clone();
    let event_handle = tokio::spawn(async move {
        // Wait until the first request has been fully processed.
        event_start_clone.notified().await;
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let value = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                    if event_writer.write_json_line(&value).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let payload = serde_json::json!({
                        "type": "event_lagged",
                        "skipped": n,
                    });
                    if event_writer.write_json_line(&payload).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
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

        // Streaming ops take ownership of the connection; handle them
        // directly so they can monitor the read half for disconnect.
        if op == "log" {
            // Unblock events now that the streaming op is starting.
            event_start.notify_waiters();
            handle_log(&request, &state_root, reader, &writer).await;
            break;
        }

        if op == "debug" {
            event_start.notify_waiters();
            handle_debug(&request, &state_root, reader, &writer).await;
            break;
        }

        if op == "chat" {
            event_start.notify_waiters();
            handle_chat(&request, &state_root, reader, &writer).await;
            break;
        }

        let _outcome = dispatch_op(&op, &request, &state_root, &shutdown_tx, &writer).await;
        // Unblock events *after* the response is written so one-shot
        // clients always read the response before any broadcast event.
        event_start.notify_waiters();
    }

    // Abort the event-forwarding subtask so it doesn't leak the write
    // half or linger waiting for broadcasts after the client is gone.
    event_handle.abort();
}

// ---------------------------------------------------------------------------
// Op dispatch
// ---------------------------------------------------------------------------

enum DispatchOutcome {
    /// Request-response: the handler wrote a reply; continue the request loop.
    Continue,
}

async fn dispatch_op(
    op: &str,
    request: &Value,
    state_root: &Path,
    shutdown_tx: &watch::Sender<bool>,
    writer: &SharedWriter,
) -> DispatchOutcome {
    match op {
        "launch" => {
            let resp = handle_launch(request, state_root, shutdown_tx).await;
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
        "stop" => {
            let resp = handle_stop(request, shutdown_tx).await;
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
        "resume" => {
            let resp = handle_resume(request, state_root, shutdown_tx).await;
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
        "ls" => {
            let resp = handle_ls(request, state_root).await;
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
        "status" => {
            let resp = handle_status(request, state_root).await;
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
        "info" => {
            let resp = handle_info(request, state_root).await;
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
        _ => {
            let resp = error_response(&format!("unknown op: {op:?}"));
            let _ = writer.write_json_line(&resp).await;
            DispatchOutcome::Continue
        }
    }
}

fn ok_response(data: Value) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("ok".to_string()));
    for (k, v) in data.as_object().into_iter().flatten() {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

fn error_response(message: &str) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("error".to_string()));
    map.insert("message".to_string(), Value::String(message.to_string()));
    Value::Object(map)
}

async fn send_debug_status(writer: &SharedWriter, stage: &str) {
    let payload = serde_json::json!({"type": "status", "stage": stage});
    let _ = writer.write_json_line(&payload).await;
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

async fn handle_launch(
    request: &Value,
    _state_root: &Path,
    shutdown_tx: &watch::Sender<bool>,
) -> Value {
    let definition = request
        .get("definition")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if definition.is_empty() {
        return error_response("missing 'definition' field");
    }

    let raw_args: Vec<String> = request
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let stage_inputs = match parse_stage_inputs(&raw_args) {
        Ok(m) => m,
        Err(e) => return error_response(&e),
    };

    if let Err(e) = config::init_global() {
        return error_response(&format!("config init: {e}"));
    }

    // Use the client's project_root if provided, otherwise resolve from cwd.
    let project_root = request
        .get("project_root")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(config::project_root);
    let definition_path =
        match crate::core::discovery::resolve_definition_path(definition, project_root.clone()) {
            Ok(p) => p,
            Err(e) => return error_response(&format!("definition not found: {e}")),
        };

    let default_client = config::global_config()
        .ok()
        .and_then(|c| c.default_client().map(String::from));
    let gremlin_def = match crate::definition::StaticDefinition::from_yaml_file(
        &definition_path,
        None,
        default_client.as_deref(),
    ) {
        Ok(d) => d,
        Err(e) => return error_response(&format!("invalid definition: {e}")),
    };

    if let Err(e) = validate_launch_inputs(&gremlin_def, &stage_inputs) {
        return error_response(&e);
    }

    let definition_name = definition_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("gremlin");

    let ephemeral = request
        .get("ephemeral")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut gremlin = match Gremlin::init(
        definition_name,
        &definition_path,
        &gremlin_def,
        &stage_inputs,
        None,
        &GremlinConfig { ephemeral },
    ) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("failed to create gremlin: {e}")),
    };

    // Record launch command in metadata.
    {
        let launch_cmd = std::iter::once(definition.to_string())
            .chain(raw_args.iter().cloned())
            .map(|arg| shell_escape(&arg))
            .collect::<Vec<_>>()
            .join(" ");
        let mut cli_meta = Map::new();
        cli_meta.insert("launch_cmd".to_string(), Value::String(launch_cmd));
        let mut meta_field = Map::new();
        meta_field.insert("cli".to_string(), Value::Object(cli_meta));
        let mut outer = Map::new();
        outer.insert("metadata".to_string(), Value::Object(meta_field));
        gremlin.state.patch(&[], &outer);
    }

    let id = gremlin.id.to_string();

    // Set up the per-gremlin log channel.
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    let (log_broadcast, _) = broadcast::channel(256);
    gremlin.runtime_config.log_tx = Some(log_tx.clone());

    // Set up interactive channels (idle until a debug session connects).
    let channels = InteractiveChannels::new();
    let (interactive_handle, interactive_session) = channels.split();
    gremlin.runtime_config.interactive = Some(interactive_handle.clone());
    gremlin.runtime_config.stream_events = Some(interactive_handle.evt_tx.clone());
    gremlin.interactive_session = Some(interactive_session);

    // Spawn the log writer: reads from the channel, appends to $state_dir/log,
    // and broadcasts to live subscribers.
    let log_path = gremlin.state.state_dir().join("log");
    spawn_log_writer(log_rx, log_path, log_broadcast.clone());

    let cancel_token = CancelToken::new();
    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.clone(),
        status: "running".to_string(),
        stage: "starting".to_string(),
        started_at: state::now_stamp(),
    });

    let id_clone = id.clone();
    let state_tx_clone = state_tx.clone();
    let shutdown_tx_clone = shutdown_tx.clone();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_for_task = aborted.clone();

    // Thread the cancel token into the gremlin so the run loop can pass it
    // to the backend.
    gremlin.cancel_token = Some(cancel_token.clone());

    // Capture state store handle and scratch_dir before gremlin is moved into the
    // spawned task.
    let store = gremlin.state.store_handle();
    let scratch_dir = gremlin.scratch_dir.path().to_path_buf();

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, _gremlin) = run_gremlin_task(gremlin, None).await;
        if !aborted_for_task.load(Ordering::Relaxed) {
            RunHandle::finish(&id_clone, result, &state_tx_clone, Some(&shutdown_tx_clone));
        }
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: Some(join_handle),
        aborted: aborted.clone(),
        state_tx,
        log_broadcast,
        interactive: interactive_handle,
        store,
        scratch_dir,
    };
    let definition_name = definition_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("gremlin")
        .to_string();

    get_run_map().lock().unwrap().insert(id.clone(), handle);

    // Emit RunStarted *before* releasing the barrier so subscribers
    // always see the start event before any terminal event.
    emit_event(DaemonEvent::RunStarted {
        id: id.clone(),
        definition: definition_name,
        stage: "starting".to_string(),
    });

    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    ok_response(serde_json::json!({"id": id}))
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

async fn handle_stop(request: &Value, _shutdown_tx: &watch::Sender<bool>) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    let run_map = get_run_map();

    // Check the run_map for a live handle.
    {
        let mut map = run_map.lock().unwrap();
        if let Some(handle) = map.get(id) {
            handle.cancel.cancel();
            if let Some(ref task) = handle.task {
                task.abort();
            }
            handle.aborted.store(true, Ordering::Relaxed);
            // Clone what we need before removing the entry.
            let state_tx = handle.state_tx.clone();
            map.remove(id);
            drop(map);
            let _ = state_tx.send(RunState {
                id: id.to_string(),
                status: "stopped".to_string(),
                stage: String::new(),
                started_at: String::new(),
            });
            // Write terminal state to disk so the aborted run isn't
            // reported as orphan and has a recorded exit_code.
            if let Ok(sd) = state::StateData::open(id) {
                sd.write_terminal_state(1);
            }
            // Reap backend resources that the aborted task would have
            // reaped in Gremlin::finish.
            reap_client_for(id);
            emit_event(DaemonEvent::RunStopped { id: id.to_string() });
            // Shutdown is signalled by handle_connection after dispatch_op
            // writes the response — do not signal here to avoid the
            // connection task being dropped before the response is sent.
            return ok_response(serde_json::json!({"id": id, "status": "stopping"}));
        }
    }

    // Not in run_map — check for orphaned or already-terminal gremlin.
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if state_file.is_file() {
        let raw = state::read_state_json(Some(&state_file));
        let status = raw.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status == "done" || status == "stopped" {
            return ok_response(serde_json::json!({
                "id": id,
                "status": status,
                "message": "already terminal"
            }));
        }
        // Orphaned: state says running but no executor entry.
        // Clean up by marking it stopped so the user can rm it.
        if let Err(e) = state::locked_update(&state_file, |data| {
            data.insert("status".to_string(), Value::String("stopped".to_string()));
            data.insert("ended_at".to_string(), Value::String(state::now_stamp()));
        }) {
            return error_response(&format!(
                "failed to update state for orphaned gremlin {id}: {e}"
            ));
        }
        return ok_response(serde_json::json!({
            "id": id,
            "status": "stopped",
            "message": "orphaned gremlin marked stopped"
        }));
    }
    error_response(&format!("unknown gremlin {id:?}"))
}

// ---------------------------------------------------------------------------
// resume
// ---------------------------------------------------------------------------

async fn handle_resume(
    request: &Value,
    state_root: &Path,
    shutdown_tx: &watch::Sender<bool>,
) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    {
        let map = get_run_map().lock().unwrap();
        if map.contains_key(id) {
            return error_response(&format!("gremlin {id} is already running"));
        }
    }

    let state_dir = state_root.join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return error_response(&format!("unknown gremlin {id:?}"));
    }

    let mut gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let status = gremlin.state.read_str("status");
    if status == "running" {
        return error_response(&format!("gremlin {id} is already running — use stop first"));
    }
    if status == "done" {
        return error_response(&format!("gremlin {id} is already done"));
    }

    let has_bail = gremlin.state.stage_error().is_some();
    if status != "stopped" && !has_bail {
        return error_response(&format!(
            "gremlin {id} cannot be resumed — status is {status:?}"
        ));
    }

    let stage = gremlin.state.read_str("stage");
    if stage.is_empty() || stage == "starting" {
        return error_response(&format!(
            "gremlin {id} has no recorded stage to resume from"
        ));
    }

    let finished = state_dir.join("finished");
    let _ = std::fs::remove_file(&finished);

    let mut fields = Map::new();
    fields.insert("status".to_string(), Value::String("running".to_string()));
    fields.insert("ended_at".to_string(), Value::Null);
    fields.insert("exit_code".to_string(), Value::Null);
    let current_attempt = gremlin.state.read_str("attempt");
    let new_attempt = format!("{current_attempt}-resume-{}", state::token_hex(2));
    fields.insert("attempt".to_string(), Value::String(new_attempt));
    gremlin.state.patch(&[], &fields);

    let resume_stage = stage.clone();
    let resume_stage_for_response = resume_stage.clone();

    // Set up the per-gremlin log channel.
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    let (log_broadcast, _) = broadcast::channel(256);
    gremlin.runtime_config.log_tx = Some(log_tx.clone());

    // Set up interactive channels (idle until a debug session connects).
    let channels = InteractiveChannels::new();
    let (interactive_handle, interactive_session) = channels.split();
    gremlin.runtime_config.interactive = Some(interactive_handle.clone());
    gremlin.runtime_config.stream_events = Some(interactive_handle.evt_tx.clone());
    gremlin.interactive_session = Some(interactive_session);

    // Spawn the log writer.
    let log_path = gremlin.state.state_dir().join("log");
    spawn_log_writer(log_rx, log_path, log_broadcast.clone());

    let cancel_token = CancelToken::new();
    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.to_string(),
        status: "running".to_string(),
        stage: stage.clone(),
        started_at: state::now_stamp(),
    });

    let id_clone = id.to_string();
    let state_tx_clone = state_tx.clone();
    let shutdown_tx_clone = shutdown_tx.clone();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_for_task = aborted.clone();

    gremlin.cancel_token = Some(cancel_token.clone());

    // Capture state store handle and scratch_dir before gremlin is moved into the
    // spawned task.
    let store = gremlin.state.store_handle();
    let scratch_dir = gremlin.scratch_dir.path().to_path_buf();
    let definition_name = gremlin.state.read_str("kind");

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, _gremlin) = run_gremlin_task(gremlin, Some(&resume_stage)).await;
        if !aborted_for_task.load(Ordering::Relaxed) {
            RunHandle::finish(&id_clone, result, &state_tx_clone, Some(&shutdown_tx_clone));
        }
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: Some(join_handle),
        aborted: aborted.clone(),
        state_tx,
        log_broadcast,
        interactive: interactive_handle,
        store,
        scratch_dir,
    };
    get_run_map().lock().unwrap().insert(id.to_string(), handle);

    // Emit RunStarted *before* releasing the barrier so subscribers
    // always see the start event before any terminal event.
    emit_event(DaemonEvent::RunStarted {
        id: id.to_string(),
        definition: definition_name,
        stage: resume_stage_for_response.clone(),
    });

    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    ok_response(serde_json::json!({"id": id, "resume_from": resume_stage_for_response}))
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

async fn handle_ls(_request: &Value, state_root: &Path) -> Value {
    let mut rows: Vec<Value> = Vec::new();

    // Live gremlins: collect (id, store) pairs under the lock, then read
    // state trees outside the lock so synchronous I/O doesn't block the
    // run map for other supervisor tasks.
    let live_ids: std::collections::HashSet<String>;
    {
        let map = get_run_map().lock().unwrap();
        live_ids = map.keys().cloned().collect();
        let pairs: Vec<(String, Arc<dyn state::StateStore + Send + Sync>)> = map
            .iter()
            .map(|(id, handle)| (id.clone(), handle.store.clone()))
            .collect();
        drop(map);

        for (id, store) in &pairs {
            let tree = store.state_tree();
            let stage = tree
                .get("stage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let started_at = tree
                .get("started_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let project = tree
                .get("project_root")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let launch = tree
                .get("metadata")
                .and_then(|v| v.get("cli"))
                .and_then(|v| v.get("launch_cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            rows.push(serde_json::json!({
                "id": id,
                "status": "running",
                "stage": stage,
                "started_at": started_at,
                "project": project,
                "launch": launch,
            }));
        }
    }

    for (id, state_json_path) in state::list_state_dirs() {
        if live_ids.contains(&id) {
            continue;
        }
        if state_root.join(&id).join("closed").exists() {
            continue;
        }
        let raw = state::read_state_json(Some(&state_json_path));
        if raw.is_empty() {
            continue;
        }
        let status = raw
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stage = raw
            .get("stage")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let started_at = raw
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let project = raw
            .get("project_root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let launch = raw
            .get("metadata")
            .and_then(|v| v.get("cli"))
            .and_then(|v| v.get("launch_cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let display_status = if status == "running" {
            "orphan".to_string()
        } else {
            status
        };

        rows.push(serde_json::json!({
            "id": id,
            "status": display_status,
            "stage": stage,
            "started_at": started_at,
            "project": project,
            "launch": launch,
        }));
    }

    rows.sort_by(|a, b| {
        a["started_at"]
            .as_str()
            .unwrap_or("")
            .cmp(b["started_at"].as_str().unwrap_or(""))
    });
    ok_response(serde_json::json!({"gremlins": rows}))
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn handle_status(request: &Value, _state_root: &Path) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    // Check the run_map first — live gremlins (including ephemeral ones
    // whose state is in a TempDir) have their store handle stored in the
    // RunHandle.
    let live = {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.store.clone())
    };

    if let Some(store) = live {
        let tree = store.state_tree();
        if tree.is_empty() {
            return error_response(&format!("gremlin {id}: state is empty"));
        }

        let state_dir = store.state_dir();
        let status = "running".to_string();
        let stage = tree
            .get("stage")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let definition = tree
            .get("definition_path")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                std::path::Path::new(s)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| {
                tree.get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            });
        let project_root = tree
            .get("project_root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let workdir = tree
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let artifact_dir = state_dir.join("artifacts");
        let started_at = tree
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ended_at = tree.get("ended_at").cloned().unwrap_or(Value::Null);
        let exit_code = tree.get("exit_code").cloned().unwrap_or(Value::Null);
        let pid = tree.get("pid").cloned().unwrap_or(Value::Null);
        let client = tree
            .get("client")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let attempt = tree
            .get("attempt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kind = tree
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        return ok_response(serde_json::json!({
            "id": id,
            "status": status,
            "stage": stage,
            "definition": definition,
            "project_root": project_root,
            "workdir": workdir,
            "state_dir": state_dir.to_string_lossy(),
            "artifact_dir": artifact_dir.to_string_lossy(),
            "started_at": started_at,
            "ended_at": ended_at,
            "exit_code": exit_code,
            "pid": pid,
            "client": client,
            "attempt": attempt,
            "kind": kind,
        }));
    }

    // Not live — fall back to Gremlin::from for persistent gremlins.
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return error_response(&format!("unknown gremlin {id:?}"));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let status = status_or_orphan(false, &gremlin);

    ok_response(serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": status,
        "stage": gremlin.state.read_str("stage"),
        "definition": definition_display_name(&gremlin),
        "project_root": gremlin.project_root.to_string_lossy(),
        "workdir": gremlin.workdir.as_ref().map(|w| w.path().to_string_lossy().to_string()).unwrap_or_default(),
        "state_dir": gremlin.state.state_dir().to_string_lossy(),
        "artifact_dir": gremlin.state.artifact_dir().to_string_lossy(),
        "started_at": gremlin.state.read_str("started_at"),
        "ended_at": gremlin.state.read_field("ended_at").unwrap_or(Value::Null),
        "exit_code": gremlin.state.read_field("exit_code").unwrap_or(Value::Null),
        "pid": gremlin.state.read_field("pid").unwrap_or(Value::Null),
        "client": gremlin.state.read_str("client"),
        "attempt": gremlin.state.read_str("attempt"),
        "kind": gremlin.state.read_str("kind"),
    }))
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

async fn handle_info(request: &Value, _state_root: &Path) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    // Check the run_map first — live gremlins (including ephemeral ones
    // whose state is in a TempDir) have their store handle stored in the
    // RunHandle.
    let live = {
        let map = get_run_map().lock().unwrap();
        map.get(id)
            .map(|h| (h.store.clone(), h.scratch_dir.clone()))
    };

    if let Some((store, scratch_dir)) = live {
        let tree = store.state_tree();
        if tree.is_empty() {
            return error_response(&format!("gremlin {id}: state is empty"));
        }

        let state_dir = store.state_dir();
        let status = "running".to_string();
        let stage = tree
            .get("stage")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let definition = tree
            .get("definition_path")
            .and_then(|v| v.as_str())
            .and_then(|s| {
                std::path::Path::new(s)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| {
                tree.get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            });
        let project_root = tree
            .get("project_root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let workdir = tree
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let artifact_dir = state_dir.join("artifacts");
        let log_file = state_dir.join("log");
        let started_at = tree
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ended_at = tree.get("ended_at").cloned().unwrap_or(Value::Null);
        let exit_code = tree.get("exit_code").cloned().unwrap_or(Value::Null);
        let pid = tree.get("pid").cloned().unwrap_or(Value::Null);
        let client = tree
            .get("client")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let attempt = tree
            .get("attempt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kind = tree
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Read bail_info from the state directory.
        let bail_info = tree
            .get("attempt")
            .and_then(|v| v.as_str())
            .filter(|a| !a.is_empty())
            .and_then(|attempt| {
                let bail_path = state_dir.join(format!("bail_{attempt}.json"));
                std::fs::read_to_string(bail_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Map<String, Value>>(&s).ok())
            })
            .map(Value::Object)
            .unwrap_or(Value::Null);

        return ok_response(serde_json::json!({
            "id": id,
            "status": status,
            "stage": stage,
            "definition": definition,
            "project_root": project_root,
            "workdir": workdir,
            "state_dir": state_dir.to_string_lossy(),
            "artifact_dir": artifact_dir.to_string_lossy(),
            "scratch_dir": scratch_dir.to_string_lossy(),
            "log_file": log_file.to_string_lossy(),
            "started_at": started_at,
            "ended_at": ended_at,
            "exit_code": exit_code,
            "pid": pid,
            "client": client,
            "attempt": attempt,
            "kind": kind,
            "bail_info": bail_info,
        }));
    }

    // Not live — fall back to Gremlin::from for persistent gremlins.
    let state_dir = config::state_root().join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return error_response(&format!("unknown gremlin {id:?}"));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let status = status_or_orphan(false, &gremlin);

    ok_response(serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": status,
        "stage": gremlin.state.read_str("stage"),
        "definition": definition_display_name(&gremlin),
        "project_root": gremlin.project_root.to_string_lossy(),
        "workdir": gremlin.workdir.as_ref().map(|w| w.path().to_string_lossy().to_string()).unwrap_or_default(),
        "state_dir": gremlin.state.state_dir().to_string_lossy(),
        "artifact_dir": gremlin.state.artifact_dir().to_string_lossy(),
        "scratch_dir": gremlin.scratch_dir.path().to_string_lossy(),
        "log_file": gremlin.state.state_dir().join("log").to_string_lossy(),
        "started_at": gremlin.state.read_str("started_at"),
        "ended_at": gremlin.state.read_field("ended_at").unwrap_or(Value::Null),
        "exit_code": gremlin.state.read_field("exit_code").unwrap_or(Value::Null),
        "pid": gremlin.state.read_field("pid").unwrap_or(Value::Null),
        "client": gremlin.state.read_str("client"),
        "attempt": gremlin.state.read_str("attempt"),
        "kind": gremlin.state.read_str("kind"),
        "bail_info": gremlin.state.stage_error().map(Value::Object).unwrap_or(Value::Null),
    }))
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

async fn handle_log(
    request: &Value,
    _state_root: &Path,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    if validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // Resolve the state directory: for live gremlins (including ephemeral),
    // use the store handle stored in the RunHandle; otherwise fall back to
    // config::state_root().
    let (state_dir, unknown) = {
        let map = get_run_map().lock().unwrap();
        if let Some(handle) = map.get(id) {
            (handle.store.state_dir().to_path_buf(), false)
        } else {
            let sd = config::state_root().join(id);
            let sf = sd.join("state.json");
            if !sd.is_dir() || !sf.is_file() {
                (PathBuf::new(), true)
            } else {
                (sd, false)
            }
        }
    };

    if unknown {
        let resp = error_response(&format!("unknown gremlin {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let log_path = state_dir.join("log");

    let follow = request
        .get("follow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Subscribe to the broadcast channel *before* replaying history so that
    // lines emitted during replay are buffered and not lost.
    let mut rx = if follow {
        get_run_map()
            .lock()
            .unwrap()
            .get(id)
            .map(|h| h.log_broadcast.subscribe())
    } else {
        None
    };

    // 1. Send historical lines from the log file.
    let mut offset: u64 = 0;
    if let Ok(content) = tokio::fs::read_to_string(&log_path).await {
        for line in content.lines() {
            let payload = serde_json::json!({
                "type": "log_line",
                "line": line,
                "offset": offset,
            });
            if writer.write_json_line(&payload).await.is_err() {
                return;
            }
            offset += 1;
        }
    }

    // 2. If follow, drain duplicates (lines broadcast during file replay),
    //    then stream new lines. Monitor the read half for client disconnect.
    if let Some(ref mut rx) = rx {
        // Drain lines that arrived during the file replay — they were already
        // sent from the file above.
        while rx.try_recv().is_ok() {}

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(line) => {
                            let payload = serde_json::json!({
                                "type": "log_line",
                                "line": line,
                                "offset": offset,
                            });
                            if writer.write_json_line(&payload).await.is_err() {
                                break;
                            }
                            offset += 1;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            let payload = serde_json::json!({
                                "type": "log_line",
                                "line": format!("[skipped {n} lines]"),
                                "offset": offset,
                            });
                            if writer.write_json_line(&payload).await.is_err() {
                                break;
                            }
                            offset += 1;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                // Client disconnect: read half yields EOF.
                _ = socket::read_json_line(&mut reader) => {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// debug
// ---------------------------------------------------------------------------

async fn handle_debug(
    request: &Value,
    _state_root: &Path,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    log::debug!("handle_debug: received debug request for {id:?}");
    send_debug_status(writer, "validating_id").await;

    if id.is_empty() {
        log::debug!("handle_debug: missing 'id' field");
        let resp = error_response("missing 'id' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    if validate_gremlin_id(id).is_err() {
        log::debug!("handle_debug: invalid gremlin id {id:?}");
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // Resolve the state directory: for live gremlins (including ephemeral),
    // use the store handle stored in the RunHandle; otherwise fall back to
    // config::state_root().
    let (state_dir, unknown) = {
        let map = get_run_map().lock().unwrap();
        if let Some(handle) = map.get(id) {
            (handle.store.state_dir().to_path_buf(), false)
        } else {
            let sd = config::state_root().join(id);
            let sf = sd.join("state.json");
            if !sd.is_dir() || !sf.is_file() {
                (PathBuf::new(), true)
            } else {
                (sd, false)
            }
        }
    };

    if unknown {
        log::debug!("handle_debug: unknown gremlin {id:?}");
        let resp = error_response(&format!("unknown gremlin {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // Validate that the gremlin is currently in an agent stage.
    // Read the current stage name from state.json and cross-reference
    // with the definition YAML to check its type.
    send_debug_status(writer, "checking_stage_type").await;
    {
        let sf = state_dir.join("state.json");
        let stage_name = if sf.is_file() {
            std::fs::read_to_string(&sf)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("stage").and_then(|s| s.as_str()).map(String::from))
                .unwrap_or_default()
        } else {
            String::new()
        };

        if stage_name.is_empty() {
            log::debug!("handle_debug: gremlin {id} has no recorded stage");
            let resp = error_response(&format!("gremlin {id} has no recorded stage"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }

        let def_path = state_dir.join("definition.yaml");
        let is_agent = def_path.is_file()
            && std::fs::read_to_string(&def_path)
                .ok()
                .and_then(|s| serde_yaml::from_str::<serde_yaml::Value>(&s).ok())
                .and_then(|root| {
                    root.get("stages")
                        .and_then(|stages| stages.as_sequence())
                        .map(|seq| {
                            seq.iter().any(|s| {
                                s.get("name").and_then(|n| n.as_str()) == Some(&stage_name)
                                    && s.get("type").and_then(|t| t.as_str()) == Some("agent")
                            })
                        })
                })
                .unwrap_or(false);

        if !is_agent {
            log::debug!(
                "handle_debug: gremlin {id} is not in an agent stage (current: {stage_name})"
            );
            let resp = error_response(&format!(
                "gremlin {id} is not in an agent stage (current: {stage_name})"
            ));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    }

    // Get the interactive handle from the run map.
    let interactive_handle = {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.interactive.clone())
    };

    let interactive_handle = match interactive_handle {
        Some(h) => h,
        None => {
            log::debug!("handle_debug: gremlin {id} is not in the run map");
            send_debug_status(writer, "not_running").await;
            let resp = error_response(&format!("gremlin {id} is not running"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    log::debug!("handle_debug: got interactive handle for {id}");
    send_debug_status(writer, "got_handle").await;

    // Subscribe to interactive events *before* triggering pause so we don't miss
    // the Ready broadcast.
    let mut evt_rx = interactive_handle.evt_tx.subscribe();

    // Signal the agent loop to pause at its next yield point.
    log::debug!("handle_debug: calling pause() for {id}");
    interactive_handle.pause.pause();
    log::debug!("handle_debug: pause() called, waiting for Ready from agent…");
    send_debug_status(writer, "sent_pause_signal").await;
    send_debug_status(writer, "waiting_for_ready").await;

    // Wait for Ready from the agent loop.
    //
    // Race against cmd_tx.closed(): if the agent session has already exited
    // (cmd_rx dropped), the cmd_tx sender will close. Without this branch
    // the select! would hang forever because evt_rx cannot close while this
    // handle_debug owns an evt_tx clone.
    let cmd_tx_closed = interactive_handle.cmd_tx.closed();
    tokio::pin!(cmd_tx_closed);
    let ready = loop {
        tokio::select! {
            result = evt_rx.recv() => {
                match result {
                    Ok(InteractiveEvent::Ready { turn }) => {
                        log::debug!("handle_debug: received Ready event (turn={turn}) for {id}");
                        break true;
                    }
                    Ok(InteractiveEvent::Ended { reason }) => {
                        log::debug!("handle_debug: received Ended event ({reason}) for {id}");
                        break false;
                    }
                    Ok(other) => {
                        log::debug!("handle_debug: ignoring event while waiting for Ready: {other:?}");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("handle_debug: broadcast lagged ({n}) for {id}, continuing");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        log::debug!("handle_debug: broadcast channel closed for {id}");
                        break false;
                    }
                }
            }
            _ = &mut cmd_tx_closed => {
                // Agent session exited — cmd_rx was dropped.
                log::debug!("handle_debug: cmd_tx closed for {id} — agent session exited");
                break false;
            }
            result = socket::read_json_line(&mut reader) => {
                // Client sent a message before Ready — decode it.
                // If it's a quit, propagate to the agent so it doesn't stay paused.
                if let Ok(Some(cmd)) = result {
                    let op = cmd.get("op").and_then(|v| v.as_str());
                    log::debug!("handle_debug: client message before Ready: op={op:?}");
                    if op == Some("quit") {
                        let _ = interactive_handle
                            .cmd_tx
                            .send(InteractiveCommand::Quit)
                            .await;
                    }
                }
                break false;
            }
        }
    };

    if !ready {
        log::debug!("handle_debug: agent did not become ready for {id}, resetting pause");
        interactive_handle.pause.reset();
        return;
    }

    // Send ready to the client.
    log::debug!("handle_debug: sending ready for {id}");
    let ready_payload = serde_json::json!({
        "type": "ready",
        "id": id,
    });
    if writer.write_json_line(&ready_payload).await.is_err() {
        log::debug!("handle_debug: failed to send ready for {id}, client disconnected");
        let _ = interactive_handle
            .cmd_tx
            .send(InteractiveCommand::Quit)
            .await;
        interactive_handle.pause.reset();
        return;
    }
    log::debug!("handle_debug: ready sent, entering command loop for {id}");

    // Bidirectional loop: read commands from client, forward to agent.
    loop {
        tokio::select! {
            // Read from the client.
            result = socket::read_json_line(&mut reader) => {
                match result {
                    Ok(Some(cmd)) => {
                        let op = cmd.get("op").and_then(|v| v.as_str()).unwrap_or("");
                        match op {
                            "talk" => {
                                let text = cmd.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if interactive_handle.cmd_tx.send(InteractiveCommand::Inject(text.to_string())).await.is_err() {
                                    break;
                                }
                            }
                            "continue" => {
                                if interactive_handle.cmd_tx.send(InteractiveCommand::RunTurn).await.is_err() {
                                    break;
                                }
                            }
                            "bail" => {
                                let reason = cmd.get("reason").and_then(|v| v.as_str()).unwrap_or("operator bailed");
                                if interactive_handle.cmd_tx.send(InteractiveCommand::Bail(reason.to_string())).await.is_err() {
                                    break;
                                }
                            }
                            "quit" => {
                                if interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await.is_err() {
                                    break;
                                }
                            }
                            _ => {
                                let resp = error_response(&format!("unknown debug op: {op:?}"));
                                let _ = writer.write_json_line(&resp).await;
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Client disconnected — send Quit so the agent resumes.
                        let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                        break;
                    }
                }
            }
            // Read events from the agent loop.
            result = evt_rx.recv() => {
                match result {
                    Ok(InteractiveEvent::Ready { .. }) => {
                        // Agent re-entered interactive mode (after RunOneTurn).
                        // No action needed — we're already in the interactive loop.
                    }
                    Ok(InteractiveEvent::TurnComplete { turn, text, tool_calls }) => {
                        let payload = serde_json::json!({"type": "turn_complete", "turn": turn, "text": text, "tool_calls": tool_calls});
                        if writer.write_json_line(&payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Ok(InteractiveEvent::Done { text, usage }) => {
                        let payload = serde_json::json!({"type": "done", "text": text, "usage": usage});
                        if writer.write_json_line(&payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Ok(InteractiveEvent::Ended { reason }) => {
                        let payload = serde_json::json!({"type": "ended", "reason": reason});
                        let _ = writer.write_json_line(&payload).await;
                        break;
                    }
                    Ok(InteractiveEvent::StreamChunk { text }) => {
                        let payload = serde_json::json!({"type": "stream_chunk", "text": text});
                        if writer.write_json_line(&payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Ok(InteractiveEvent::ReasoningChunk { text }) => {
                        let payload = serde_json::json!({"type": "reasoning_chunk", "text": text});
                        if writer.write_json_line(&payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Ok(InteractiveEvent::ToolResult { name, output }) => {
                        let payload = serde_json::json!({"type": "tool_result", "name": name, "output": output});
                        if writer.write_json_line(&payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Clear the pause signal so the agent loop resumes normal operation.
    interactive_handle.pause.reset();
}

// ---------------------------------------------------------------------------
// chat
// ---------------------------------------------------------------------------

const CHAT_SYSTEM_PROMPT: &str = r#"You are a conversational AI assistant integrated into the gremlins TUI. You help the operator develop and operate gremlin pipelines.

## Your role

You are a chat agent — respond conversationally to the operator's messages. When you have answered the operator's question completely, call Done to signal the end of your turn. When you need information, use your tools (Read, Grep, Glob, Bash, etc.) proactively — the operator expects you to look things up rather than ask them to provide information you can find yourself.

## Gremlins domain

Gremlins is a framework for running AI-powered pipelines defined in YAML files under the `.gremlins/` directory.

### Key concepts

- **Definitions** (`.gremlins/*.yaml`): Pipeline files declaring stages, their types, prompts, commands, and artifact wiring.
- **Stages**: `agent` (LLM call), `exec` (shell command), `sequence` (loop), `parallel` (fan-out).
- **Artifacts**: Files exchanged between stages via `artifact://` URIs. Stored in the gremlin's artifact directory.
- **Bail**: A stage can bail to request operator intervention. The run pauses and waits for the operator to resolve the issue and resume.
- **Worktrees**: Each gremlin run gets a detached git worktree at the commit it was launched from. The worktree is the `cwd` for all stage commands.
- **State**: Each run has a state directory with `state.json`, logs, artifacts, and a hermetic definition snapshot.
- **Overlay**: The `.gremlins/` directory in the project root contains pipeline definitions and tool scripts.

### Common commands

- `gremlins launch <definition>` — start a pipeline run
- `gremlins ls` — list all runs
- `gremlins info <id>` — show run details
- `gremlins stop <id>` — stop a run
- `gremlins resume <id>` — resume a paused/bailed run
- `gremlins debug <id>` — attach interactively to an agent stage
- `gremlins rm <id>` — remove a completed run

## Your tools

You have access to standard tools: Read, Write, Edit, Grep, Glob, Bash, Task. Use them to help the operator explore the codebase, edit files, run commands, and manage gremlin pipelines.

## Guidelines

- Be concise and direct. The operator is a developer who knows the codebase.
- When reading files, use offset/limit for large files.
- When editing, make targeted replacements with enough context for uniqueness.
- Use `make test` to run tests, `make check` for linting.
- Work in the project root directory unless told otherwise."#;

async fn handle_chat(
    request: &Value,
    state_root: &Path,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    log::debug!("handle_chat: received chat request");

    // Extract text and history from the request.
    let text = request.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        let resp = error_response("missing 'text' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let history: Vec<Value> = request
        .get("history")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // 1. Init config
    if let Err(e) = config::init_global() {
        let resp = error_response(&format!("config init: {e}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // 2. Get default client
    let default_client = match config::global_config() {
        Ok(cfg) => match cfg.default_client() {
            Some(c) => c.to_string(),
            None => {
                let resp = error_response("no default client configured");
                let _ = writer.write_json_line(&resp).await;
                return;
            }
        },
        Err(e) => {
            let resp = error_response(&format!("config error: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // 3. Build the conversation transcript from history.
    let transcript = render_history_transcript(&history);
    let system_prompt = format!("{CHAT_SYSTEM_PROMPT}\n\n{transcript}");

    // 4. Build programmatic definition — per-message ephemeral stage.
    let stage = match crate::builders::AgentBuilder::new("chat")
        .prompt(text)
        .option("system_prompt", system_prompt)
        .build()
    {
        Ok(s) => s,
        Err(e) => {
            let resp = error_response(&format!("failed to build agent stage: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    let definition = match crate::builders::DefinitionBuilder::new("chat", &default_client)
        .stage(stage)
        .build()
    {
        Ok(d) => d,
        Err(e) => {
            let resp = error_response(&format!("failed to build definition: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // 5. Create chat gremlin
    let mut gremlin = match create_chat_gremlin(&definition, &default_client, state_root) {
        Ok(g) => g,
        Err(e) => {
            let resp = error_response(&e);
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    let id = gremlin.id.to_string();

    // 6. Set up log channel
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    let (log_broadcast, _) = broadcast::channel(256);
    gremlin.runtime_config.log_tx = Some(log_tx.clone());

    // Spawn log writer
    let log_path = gremlin.state.state_dir().join("log");
    spawn_log_writer(log_rx, log_path, log_broadcast.clone());

    // 7. Set up stream_events for forwarding to the TUI.
    //    No interactive session — the agent runs to Done.
    let (stream_tx, mut stream_rx) = broadcast::channel::<InteractiveEvent>(256);
    gremlin.runtime_config.stream_events = Some(stream_tx.clone());

    // 8. Spawn run task
    let cancel_token = CancelToken::new();
    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.clone(),
        status: "running".to_string(),
        stage: "starting".to_string(),
        started_at: state::now_stamp(),
    });

    let id_clone = id.clone();
    let state_tx_clone = state_tx.clone();
    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_for_task = aborted.clone();

    gremlin.cancel_token = Some(cancel_token.clone());

    // Capture state store handle and scratch_dir before gremlin is moved.
    let store = gremlin.state.store_handle();
    let scratch_dir = gremlin.scratch_dir.path().to_path_buf();

    let join_handle = tokio::spawn(async move {
        let (result, _gremlin) = run_gremlin_task(gremlin, None).await;
        if !aborted_for_task.load(Ordering::Relaxed) {
            RunHandle::finish(
                &id_clone,
                result,
                &state_tx_clone,
                None::<&watch::Sender<bool>>,
            );
        }
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: Some(join_handle),
        aborted: aborted.clone(),
        state_tx,
        log_broadcast,
        interactive: InteractiveChannels::new().split().0,
        store,
        scratch_dir,
    };
    get_run_map().lock().unwrap().insert(id.clone(), handle);

    emit_event(DaemonEvent::RunStarted {
        id: id.clone(),
        definition: "chat".to_string(),
        stage: "starting".to_string(),
    });

    // 9. Forward stream events to the TUI until Done/Ended.
    //    Also monitor the read half for client disconnect.
    log::debug!("handle_chat: forwarding stream events for {id}");
    let mut early_exit = true;
    loop {
        tokio::select! {
            result = stream_rx.recv() => {
                match result {
                    Ok(InteractiveEvent::StreamChunk { text }) => {
                        let payload = serde_json::json!({"type": "stream_chunk", "text": text});
                        if writer.write_json_line(&payload).await.is_err() {
                            break;
                        }
                    }
                    Ok(InteractiveEvent::ReasoningChunk { text }) => {
                        let payload = serde_json::json!({"type": "reasoning_chunk", "text": text});
                        if writer.write_json_line(&payload).await.is_err() {
                            break;
                        }
                    }
                    Ok(InteractiveEvent::ToolResult { name, output }) => {
                        let payload = serde_json::json!({"type": "tool_result", "name": name, "output": output});
                        if writer.write_json_line(&payload).await.is_err() {
                            break;
                        }
                    }
                    Ok(InteractiveEvent::TurnComplete { turn, text, tool_calls }) => {
                        let payload = serde_json::json!({"type": "turn_complete", "turn": turn, "text": text, "tool_calls": tool_calls});
                        if writer.write_json_line(&payload).await.is_err() {
                            break;
                        }
                    }
                    Ok(InteractiveEvent::Done { text, usage }) => {
                        early_exit = false;
                        let payload = serde_json::json!({"type": "done", "text": text, "usage": usage});
                        let _ = writer.write_json_line(&payload).await;
                        break;
                    }
                    Ok(InteractiveEvent::Ended { reason }) => {
                        early_exit = false;
                        let payload = serde_json::json!({"type": "ended", "reason": reason});
                        let _ = writer.write_json_line(&payload).await;
                        break;
                    }
                    Ok(InteractiveEvent::Ready { .. }) => {
                        // Ignore Ready in per-message mode — the agent
                        // runs to completion without pausing.
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        early_exit = false;
                        break;
                    }
                }
            }
            _ = socket::read_json_line(&mut reader) => {
                // Client disconnected.
                break;
            }
        }
    }

    // Cancel the gremlin on early exit (client disconnect, write error).
    if early_exit {
        if let Some(handle) = get_run_map().lock().unwrap().get(id.as_str()) {
            handle.cancel.cancel();
        }
    }

    log::debug!("handle_chat: done forwarding for {id}");
}

/// Render conversation history into a compact transcript block for the
/// system prompt.
fn render_history_transcript(history: &[Value]) -> String {
    if history.is_empty() {
        return String::new();
    }
    let mut lines = vec!["## Conversation history".to_string(), String::new()];
    for entry in history {
        let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let label = match role {
            "user" => "Operator",
            "assistant" => "Assistant",
            _ => role,
        };
        lines.push(format!("{label}: {content}"));
        lines.push(String::new());
    }
    lines.join("\n")
}

static CHAT_SEQ: AtomicUsize = AtomicUsize::new(0);

fn create_chat_gremlin(
    definition: &crate::definition::StaticDefinition,
    default_client: &str,
    _state_root: &Path,
) -> Result<Gremlin, String> {
    use crate::clients::client::Client;
    use crate::config;
    use crate::definition::GremlinDefinition;
    use crate::executor::gremlin::{validate_gremlin_id, Gremlin, RuntimeConfig, ScratchDir};
    use crate::executor::state::{self, BlobMode, StateData};
    use serde_json::{Map, Value};
    use std::collections::HashMap;

    let project_root = config::project_root();

    // Generate ID — use an atomic counter so concurrent chat sessions
    // never collide. Chat gremlins are ephemeral so they never appear
    // under config::state_root().
    let gremlin_id = loop {
        let seq = CHAT_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("chat-{seq:04x}");
        let id = validate_gremlin_id(&candidate).map_err(|e| format!("invalid id: {e}"))?;
        // Double-check the run_map in case a previous chat session with
        // the same counter value is still alive (extremely unlikely but
        // cheap to guard against).
        if !get_run_map().lock().unwrap().contains_key(id.as_str()) {
            break id;
        }
    };

    // Build initial state and create StateData (which writes state.json).
    let mut initial = Map::new();
    initial.insert("id".to_string(), Value::String(gremlin_id.to_string()));
    initial.insert("kind".to_string(), Value::String("chat".to_string()));
    initial.insert(
        "project_root".to_string(),
        Value::String(project_root.to_string_lossy().into_owned()),
    );
    initial.insert("workdir".to_string(), Value::String(String::new()));
    initial.insert("status".to_string(), Value::String("running".to_string()));
    initial.insert("started_at".to_string(), Value::String(state::now_stamp()));
    initial.insert(
        "client".to_string(),
        Value::String(default_client.to_string()),
    );
    initial.insert("stage".to_string(), Value::String("starting".to_string()));
    initial.insert("pid".to_string(), Value::from(std::process::id() as i64));
    initial.insert("attempt".to_string(), Value::String(String::new()));
    initial.insert("exit_code".to_string(), Value::Null);
    initial.insert("definition_path".to_string(), Value::String(String::new()));
    initial.insert("description".to_string(), Value::String(String::new()));
    initial.insert("parent_id".to_string(), Value::String(String::new()));
    initial.insert("definition_args".to_string(), Value::Array(Vec::new()));
    initial.insert("stage_inputs".to_string(), Value::Object(Map::new()));
    initial.insert("group_name".to_string(), Value::String(String::new()));
    initial.insert("child_key".to_string(), Value::String(String::new()));
    initial.insert("metadata".to_string(), Value::Object(Map::new()));

    let state = StateData::new(gremlin_id.as_str(), &initial, /* ephemeral */ true)
        .map_err(|e| format!("failed to write state: {e}"))?;

    // Write definition.yaml via state blob.
    let yaml_bytes = definition
        .serialize()
        .map_err(|e| format!("failed to serialize definition: {e}"))?;
    {
        let mut blob = state
            .open_blob("definition.yaml", BlobMode::Write)
            .map_err(|e| format!("failed to write definition: {e}"))?;
        std::io::Write::write_all(&mut blob, &yaml_bytes)
            .map_err(|e| format!("failed to write definition: {e}"))?;
    }
    let def_path = state.state_dir().join("definition.yaml");
    let def_path = def_path.canonicalize().unwrap_or(def_path);

    // Create empty log file via state blob.
    let _ = state
        .open_blob("log", BlobMode::Write)
        .map_err(|e| format!("failed to create log: {e}"))?;

    let runtime_config = RuntimeConfig::snapshot();
    let client = Client::parse(default_client)
        .map_err(|e| format!("invalid default client '{default_client}': {e}"))?;

    let scratch_dir = ScratchDir::Persistent(config::scratch_root(Some(gremlin_id.as_str())));

    Ok(Gremlin {
        id: gremlin_id,
        definition_path: Some(def_path),
        client_override: None,
        definition: Box::new(definition.clone()),
        workdir: None,
        project_root,
        state,
        env: HashMap::new(),
        client,
        loop_iter: "1".to_string(),
        stage_inputs: HashMap::new(),
        runtime_config,
        cancel_token: None,
        interactive_session: None,
        scratch_dir,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn run_gremlin_task(mut gremlin: Gremlin, resume_from: Option<&str>) -> (i32, Gremlin) {
    let exit_code = match gremlin.run(resume_from).await {
        Ok(ec) => ec,
        Err(e) => {
            log::error!("gremlin {}: {e}", gremlin.id.as_str());
            if let Some(tx) = &gremlin.runtime_config.log_tx {
                let _ = tx.send(format!("error: {e}"));
            }
            gremlin.state.write_terminal_state(1);
            1
        }
    };
    (exit_code, gremlin)
}

/// Spawn a background task that reads from the log channel, appends each
/// line to `log_path`, and broadcasts it to live subscribers.
fn spawn_log_writer(
    mut log_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    log_path: PathBuf,
    broadcast_tx: broadcast::Sender<String>,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;

        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => Some(f),
            Err(e) => {
                log::error!("log writer: failed to open {}: {e}", log_path.display());
                None
            }
        };

        while let Some(line) = log_rx.recv().await {
            let ts = crate::executor::state::now_stamp_millis();
            // Stamp each newline-separated part so every physical log line
            // carries a timestamp, even when a single message contains
            // embedded newlines (e.g. stage names or error text).
            for part in line.split('\n') {
                let stamped = format!("{ts} {part}");
                if let Some(ref mut f) = file {
                    let _ = f.write_all(stamped.as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                    let _ = f.flush().await;
                }
                // Broadcast to live subscribers — ignore errors (no subscribers).
                let _ = broadcast_tx.send(stamped);
            }
        }
    });
}

/// When the run_map says the gremlin is live, always report "running".
/// When it's not live and state.json says "running", it's orphaned.
/// Otherwise return the status from state.json as-is.
fn status_or_orphan(is_live: bool, gremlin: &Gremlin) -> String {
    if is_live {
        return "running".to_string();
    }
    let status = gremlin.state.read_str("status");
    if status == "running" {
        return "orphan".to_string();
    }
    status
}

fn definition_display_name(gremlin: &Gremlin) -> String {
    let recorded = gremlin.state.read_str("definition_path");
    if let Some(stem) = std::path::Path::new(&recorded)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    {
        return stem.to_string();
    }
    gremlin.state.read_str("kind")
}

fn shell_escape(arg: &str) -> String {
    let needs_quoting = arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | '$' | '`' | '\''));
    if !needs_quoting {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_stage_inputs(raw: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    let mut i = 0;
    while i < raw.len() {
        let key = raw[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected --key value, got {}", raw[i]))?;
        if key.is_empty() {
            return Err("-- with no key name".to_string());
        }
        i += 1;
        let value = if i < raw.len() && !raw[i].starts_with("--") {
            let v = raw[i].clone();
            i += 1;
            v
        } else {
            return Err(format!(
                "--{key} requires a value (found {})",
                if i < raw.len() {
                    &raw[i]
                } else {
                    "end of arguments"
                }
            ));
        };
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// launch_child / stop_child — public API for the parallel executor
// ---------------------------------------------------------------------------

/// Result of [`launch_child`]: a state watch receiver that fires when the
/// child reaches a terminal state, and a oneshot receiver for the child
/// [`Gremlin`] after its task completes.
pub(crate) struct LaunchResult {
    pub state_rx: watch::Receiver<RunState>,
    pub gremlin_rx: tokio::sync::oneshot::Receiver<Gremlin>,
}

/// Launch a pre-configured child gremlin, registering it in the run_map so
/// it appears in `gremlins ls` with live status.
///
/// The child must already be forked (artifacts copied, worktree branched,
/// state written). This function creates cancel and state-watch channels,
/// spawns a tokio task for [`run_gremlin_task`], inserts a [`RunHandle`]
/// into the global run map, and returns a [`LaunchResult`] with receivers
/// that fire when the child reaches a terminal state.
///
/// Unlike [`handle_launch`], this does *not* generate an id, resolve a
/// definition, or call [`Gremlin::init`] — the gremlin is ready before the
/// call and is consumed by it.
///
/// This function is synchronous (no `.await`) so its caller — the parallel
/// executor — does not produce a non-`Send` future. The run_map uses a
/// `std::sync::Mutex` so `lock()` never blocks on an async runtime.
pub(crate) fn launch_child(mut gremlin: Gremlin) -> LaunchResult {
    let id = gremlin.id.to_string();

    // Set up the per-gremlin log channel.
    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
    let (log_broadcast, _) = broadcast::channel(256);
    gremlin.runtime_config.log_tx = Some(log_tx.clone());

    // Set up interactive channels (idle until a debug session connects).
    let channels = InteractiveChannels::new();
    let (interactive_handle, interactive_session) = channels.split();
    gremlin.runtime_config.interactive = Some(interactive_handle.clone());
    gremlin.runtime_config.stream_events = Some(interactive_handle.evt_tx.clone());
    gremlin.interactive_session = Some(interactive_session);

    // Spawn the log writer.
    let log_path = gremlin.state.state_dir().join("log");
    spawn_log_writer(log_rx, log_path, log_broadcast.clone());

    // Reuse the cancel token inherited from the parent via Gremlin::fork;
    // create a fresh one only when there is no parent token.
    let cancel_token = gremlin
        .cancel_token
        .clone()
        .unwrap_or_else(CancelToken::new);
    let (state_tx, state_rx) = watch::channel(RunState {
        id: id.clone(),
        status: "running".to_string(),
        stage: "starting".to_string(),
        started_at: state::now_stamp(),
    });

    let state_tx_clone = state_tx.clone();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_for_task = aborted.clone();

    // cancel_token is already set on the gremlin (inherited via fork or
    // just created above).  No need to overwrite.

    let id_clone = id.clone();

    // Capture state store handle and scratch_dir before gremlin is moved into the
    // spawned task.
    let store = gremlin.state.store_handle();
    let scratch_dir = gremlin.scratch_dir.path().to_path_buf();

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let (gremlin_tx, gremlin_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, gremlin) = run_gremlin_task(gremlin, None).await;
        // Send the gremlin back to the parallel coordinator so it can
        // keep TempDirs alive during post-processing.
        let _ = gremlin_tx.send(gremlin);
        if !aborted_for_task.load(Ordering::Relaxed) {
            // Don't signal shutdown — the parent gremlin is still running.
            RunHandle::finish(
                &id_clone,
                result,
                &state_tx_clone,
                None::<&watch::Sender<bool>>,
            );
        }
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: Some(join_handle),
        aborted: aborted.clone(),
        state_tx,
        log_broadcast,
        interactive: interactive_handle,
        store,
        scratch_dir,
    };
    // Insert into RUN_MAP *after* spawning so the JoinHandle is present
    // from the moment the entry exists.
    get_run_map().lock().unwrap().insert(id.clone(), handle);
    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    LaunchResult {
        state_rx,
        gremlin_rx,
    }
}

/// Stop a child gremlin by aborting its task and cleaning up.
///
/// A no-op when the child is not in the run map (already finished or never
/// launched).
pub(crate) async fn stop_child(id: &str) {
    let run_map = get_run_map();
    let state_tx = {
        let mut map = run_map.lock().unwrap();
        match map.get(id) {
            Some(handle) => {
                handle.cancel.cancel();
                if let Some(ref task) = handle.task {
                    task.abort();
                }
                handle.aborted.store(true, Ordering::Relaxed);
                // Clone what we need before removing the entry.
                let state_tx = handle.state_tx.clone();
                // Remove from run_map and broadcast terminal state while we
                // still hold the lock — no race window for a duplicate finish.
                map.remove(id);
                Some(state_tx)
            }
            None => None,
        }
    };
    if let Some(state_tx) = state_tx {
        // Don't signal shutdown — the parent gremlin is still running.
        let _ = state_tx.send(RunState {
            id: id.to_string(),
            status: "stopped".to_string(),
            stage: String::new(),
            started_at: String::new(),
        });
        // Write terminal state to disk so the aborted child isn't
        // reported as orphan and has a recorded exit_code.
        if let Ok(sd) = state::StateData::open(id) {
            sd.write_terminal_state(1);
        }
        // Reap backend resources that the aborted task would have
        // reaped in Gremlin::finish.
        reap_client_for(id);
    }
}

/// Reap backend resources for an aborted gremlin by reading its client
/// spec from state.json and calling [`Client::reap_all`].
///
/// If the client spec cannot be read or parsed this is a silent no-op —
/// the gremlin may not have started running yet.
fn reap_client_for(id: &str) {
    let spec = state::StateData::open(id)
        .map(|sd| sd.read_str("client"))
        .unwrap_or_default();
    if spec.is_empty() {
        return;
    }
    match Client::parse(&spec) {
        Ok(client) => client.reap_all(id),
        Err(e) => log::warn!("reap_client_for {id}: failed to parse client spec {spec:?}: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Shutdown helper
// ---------------------------------------------------------------------------

/// Signal shutdown when there are no running gremlins and no open
/// connections. Safe to call from any task.
fn check_shutdown(shutdown_tx: &watch::Sender<bool>) {
    let runs_empty = get_run_map().lock().unwrap().is_empty();
    let conns_zero = get_connection_count().load(Ordering::Relaxed) == 0;
    if runs_empty && conns_zero {
        let _ = shutdown_tx.send(true);
    }
}

fn validate_launch_inputs(
    gremlin_def: &crate::definition::StaticDefinition,
    stage_inputs: &HashMap<String, String>,
) -> Result<(), String> {
    use crate::schemas::bootstrap;
    match &gremlin_def.bootstrap.source {
        Some(source) => {
            let declared: Vec<String> = source.all_sources();
            for key in stage_inputs.keys() {
                if !declared.iter().any(|d| d == key) {
                    return Err(format!(
                        "unknown input {key:?} — definition declares sources: {}",
                        declared.join(", ")
                    ));
                }
            }
            bootstrap::validate_source_values(source, stage_inputs).map_err(|e| format!("{e}"))?;
        }
        None => {
            if !stage_inputs.is_empty() {
                return Err(
                    "definition declares no bootstrap.source — no --key args allowed".to_string(),
                );
            }
        }
    }
    Ok(())
}
