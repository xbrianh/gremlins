//! Single-instance executor supervisor.
//!
//! The supervisor owns the run map and the accept loop. It holds a
//! `HashMap<String, RunHandle>` and dispatches socket requests to the
//! appropriate handler.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::clients::agent_loop::CancelToken;
use crate::clients::interactive::{
    InteractiveChannels, InteractiveCommand, InteractiveEvent, InteractiveHandle,
};
use crate::config;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::socket::{self, GremlinsDaemonLock};
use crate::executor::state;

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
}

impl RunHandle {
    /// Post-run cleanup: remove from run_map, broadcast terminal state,
    /// optionally signal shutdown when the run map empties.
    fn finish(
        id: &str,
        result: i32,
        state_tx: &watch::Sender<RunState>,
        shutdown_tx: Option<&watch::Sender<bool>>,
    ) {
        // Send terminal state *before* removing from the run_map.
        // If we remove first, the RunHandle (and its state_tx) is
        // dropped, leaving only the caller's clone.  The receiver
        // might see all senders gone before observing the change.
        let _ = state_tx.send(RunState {
            id: id.to_string(),
            status: if result == 0 { "done" } else { "stopped" }.to_string(),
            stage: String::new(),
            started_at: String::new(),
        });

        let run_map = get_run_map();
        let is_empty = {
            let mut map = run_map.lock().unwrap();
            // Only proceed if we actually removed an entry — duplicate
            // finish calls (from handle_stop racing the spawned task) are
            // harmless no-ops.
            if map.remove(id).is_none() {
                return;
            }
            map.is_empty()
        };
        if is_empty {
            if let Some(tx) = shutdown_tx {
                let _ = tx.send(true);
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
/// gremlin drains), the file descriptor closes and the kernel releases the
/// lock automatically.
pub async fn run_supervisor(
    listener: UnixListener,
    state_root: PathBuf,
    _lock: GremlinsDaemonLock,
) {
    // Initialise the global run_map so launch_child/stop_child work.
    get_run_map();
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
                        let state_root = state_root.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
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
    socket::unlink_socket(&state_root);
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state_root: PathBuf,
    shutdown_tx: watch::Sender<bool>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

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

        // Streaming ops take ownership of the connection; handle them
        // directly so they can monitor the read half for disconnect.
        if op == "log" {
            handle_log(&request, &state_root, reader, &mut write_half).await;
            break;
        }

        if op == "debug" {
            handle_debug(&request, &state_root, reader, &mut write_half).await;
            break;
        }

        let _outcome = dispatch_op(&op, &request, &state_root, &shutdown_tx, &mut write_half).await;

        if matches!(op.as_str(), "launch" | "stop" | "resume")
            && get_run_map().lock().unwrap().is_empty()
        {
            let _ = shutdown_tx.send(true);
        }
    }
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
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> DispatchOutcome {
    match op {
        "launch" => {
            let resp = handle_launch(request, state_root, shutdown_tx).await;
            let _ = socket::write_json_line(write_half, &resp).await;
            DispatchOutcome::Continue
        }
        "stop" => {
            let resp = handle_stop(request, shutdown_tx).await;
            let _ = socket::write_json_line(write_half, &resp).await;
            DispatchOutcome::Continue
        }
        "resume" => {
            let resp = handle_resume(request, state_root, shutdown_tx).await;
            let _ = socket::write_json_line(write_half, &resp).await;
            DispatchOutcome::Continue
        }
        "ls" => {
            let resp = handle_ls(request, state_root).await;
            let _ = socket::write_json_line(write_half, &resp).await;
            DispatchOutcome::Continue
        }
        "status" => {
            let resp = handle_status(request, state_root).await;
            let _ = socket::write_json_line(write_half, &resp).await;
            DispatchOutcome::Continue
        }
        "info" => {
            let resp = handle_info(request, state_root).await;
            let _ = socket::write_json_line(write_half, &resp).await;
            DispatchOutcome::Continue
        }
        _ => {
            let resp = error_response(&format!("unknown op: {op:?}"));
            let _ = socket::write_json_line(write_half, &resp).await;
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

    let base_ref = gremlin_def.base_ref.clone();
    let base_ref_sha = if base_ref.is_empty() || base_ref == "HEAD" {
        String::new()
    } else {
        match crate::core::git::resolve_base_ref(&base_ref, Some(&project_root)) {
            Ok((_name, sha)) => sha,
            Err(e) => return error_response(&format!("failed to resolve base_ref: {e}")),
        }
    };
    let base_ref_opt = if base_ref.is_empty() {
        None
    } else {
        Some(base_ref.as_str())
    };
    let base_ref_sha_opt = if base_ref_sha.is_empty() {
        None
    } else {
        Some(base_ref_sha.as_str())
    };

    let mut gremlin = match Gremlin::init(
        definition_name,
        &definition_path,
        &gremlin_def,
        &stage_inputs,
        None,
        None,
        base_ref_opt,
        base_ref_sha_opt,
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
    gremlin.interactive_session = Some(interactive_session);

    // Spawn the log writer: reads from the channel, appends to $state_dir/log,
    // and broadcasts to live subscribers.
    let log_path = gremlin.state_dir.join("log");
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

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, _registry) = run_gremlin_task(gremlin, None).await;
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
    };
    get_run_map().lock().unwrap().insert(id.clone(), handle);
    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    ok_response(serde_json::json!({"id": id}))
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

async fn handle_stop(request: &Value, shutdown_tx: &watch::Sender<bool>) -> Value {
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
            // Remove from run_map and broadcast terminal state while we
            // still hold the lock — no race window for a duplicate finish.
            map.remove(id);
            let is_empty = map.is_empty();
            drop(map);
            let _ = state_tx.send(RunState {
                id: id.to_string(),
                status: "stopped".to_string(),
                stage: String::new(),
                started_at: String::new(),
            });
            if is_empty {
                let _ = shutdown_tx.send(true);
            }
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

    let has_bail = gremlin.state.read_bail_info().is_some();
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
    gremlin.interactive_session = Some(interactive_session);

    // Spawn the log writer.
    let log_path = gremlin.state_dir.join("log");
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

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, _registry) = run_gremlin_task(gremlin, Some(&resume_stage)).await;
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
    };
    get_run_map().lock().unwrap().insert(id.to_string(), handle);
    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    ok_response(serde_json::json!({"id": id, "resume_from": resume_stage_for_response}))
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

async fn handle_ls(_request: &Value, state_root: &Path) -> Value {
    let live: HashMap<String, String> = {
        let map = get_run_map().lock().unwrap();
        map.keys()
            .map(|id| (id.clone(), "running".to_string()))
            .collect()
    };

    let mut rows: Vec<Value> = Vec::new();

    for (id, status) in &live {
        let state_dir = state_root.join(id);
        let state_file = state_dir.join("state.json");
        let raw = state::read_state_json(Some(&state_file));
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

        rows.push(serde_json::json!({
            "id": id,
            "status": status,
            "stage": stage,
            "started_at": started_at,
            "project": project,
            "launch": launch,
        }));
    }

    for (id, state_json_path) in state::list_state_dirs() {
        if live.contains_key(&id) {
            continue;
        }
        let Some(state_dir) = state_json_path.parent() else {
            continue;
        };
        if state_dir.join("closed").is_file() {
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

    ok_response(serde_json::json!({"gremlins": rows}))
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn handle_status(request: &Value, state_root: &Path) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    let state_dir = state_root.join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return error_response(&format!("unknown gremlin {id:?}"));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let is_live = get_run_map().lock().unwrap().contains_key(id);

    let status = status_or_orphan(is_live, &gremlin);

    ok_response(serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": status,
        "stage": gremlin.state.read_str("stage"),
        "definition": definition_display_name(&gremlin),
        "project_root": gremlin.project_root.to_string_lossy(),
        "workdir": gremlin.worktree.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        "state_dir": gremlin.state_dir.to_string_lossy(),
        "artifact_dir": gremlin.artifact_dir.to_string_lossy(),
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

async fn handle_info(request: &Value, state_root: &Path) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    let state_dir = state_root.join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        return error_response(&format!("unknown gremlin {id:?}"));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let is_live = get_run_map().lock().unwrap().contains_key(id);

    let status = status_or_orphan(is_live, &gremlin);

    ok_response(serde_json::json!({
        "id": gremlin.id.as_str(),
        "status": status,
        "stage": gremlin.state.read_str("stage"),
        "definition": definition_display_name(&gremlin),
        "project_root": gremlin.project_root.to_string_lossy(),
        "workdir": gremlin.worktree.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        "state_dir": gremlin.state_dir.to_string_lossy(),
        "artifact_dir": gremlin.artifact_dir.to_string_lossy(),
        "scratch_dir": config::scratch_root(Some(id)).to_string_lossy(),
        "log_file": gremlin.state_dir.join("log").to_string_lossy(),
        "started_at": gremlin.state.read_str("started_at"),
        "ended_at": gremlin.state.read_field("ended_at").unwrap_or(Value::Null),
        "exit_code": gremlin.state.read_field("exit_code").unwrap_or(Value::Null),
        "pid": gremlin.state.read_field("pid").unwrap_or(Value::Null),
        "client": gremlin.state.read_str("client"),
        "attempt": gremlin.state.read_str("attempt"),
        "kind": gremlin.state.read_str("kind"),
        "base_ref": gremlin.base_ref,
        "worktree_base": gremlin.base_ref_sha,
        "bail_info": gremlin.state.read_bail_info().map(Value::Object).unwrap_or(Value::Null),
    }))
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

async fn handle_log(
    request: &Value,
    state_root: &Path,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    if validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    let state_dir = state_root.join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        let resp = error_response(&format!("unknown gremlin {id:?}"));
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    let follow = request
        .get("follow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let log_path = state_dir.join("log");

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
            if socket::write_json_line(write_half, &payload).await.is_err() {
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
                            if socket::write_json_line(write_half, &payload).await.is_err() {
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
                            if socket::write_json_line(write_half, &payload).await.is_err() {
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
    state_root: &Path,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    if validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    let state_dir = state_root.join(id);
    let state_file = state_dir.join("state.json");
    if !state_dir.is_dir() || !state_file.is_file() {
        let resp = error_response(&format!("unknown gremlin {id:?}"));
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    // Validate that the gremlin is currently in an agent stage.
    // Read the current stage name from state.json and cross-reference
    // with the definition YAML to check its type.
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
            let resp = error_response(&format!("gremlin {id} has no recorded stage"));
            let _ = socket::write_json_line(write_half, &resp).await;
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
            let resp = error_response(&format!(
                "gremlin {id} is not in an agent stage (current: {stage_name})"
            ));
            let _ = socket::write_json_line(write_half, &resp).await;
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
            let resp = error_response(&format!("gremlin {id} is not running"));
            let _ = socket::write_json_line(write_half, &resp).await;
            return;
        }
    };

    // Subscribe to interactive events *before* sending Pause so we don't miss
    // the Ready broadcast.
    let mut evt_rx = interactive_handle.evt_tx.subscribe();

    // Send Pause command to the agent loop.
    if interactive_handle
        .cmd_tx
        .send(InteractiveCommand::Pause)
        .await
        .is_err()
    {
        let resp = error_response("failed to send pause command — agent loop may have exited");
        let _ = socket::write_json_line(write_half, &resp).await;
        return;
    }

    // Wait for Ready from the agent loop.
    let ready = loop {
        tokio::select! {
            result = evt_rx.recv() => {
                match result {
                    Ok(InteractiveEvent::Ready { .. }) => break true,
                    Ok(InteractiveEvent::Ended { .. }) => break false,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break false,
                    _ => continue,
                }
            }
            _ = socket::read_json_line(&mut reader) => {
                // Client disconnected before Ready.
                break false;
            }
        }
    };

    if !ready {
        return;
    }

    // Send debug_ready to the client.
    let ready_payload = serde_json::json!({
        "type": "debug_ready",
        "id": id,
    });
    if socket::write_json_line(write_half, &ready_payload)
        .await
        .is_err()
    {
        let _ = interactive_handle
            .cmd_tx
            .send(InteractiveCommand::Quit)
            .await;
        return;
    }

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
                                let _ = socket::write_json_line(write_half, &resp).await;
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
                    Ok(InteractiveEvent::TurnComplete { .. }) => {
                        let payload = serde_json::json!({"type": "debug_turn_complete"});
                        if socket::write_json_line(write_half, &payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Ok(InteractiveEvent::Done { .. }) => {
                        let payload = serde_json::json!({"type": "debug_done"});
                        if socket::write_json_line(write_half, &payload).await.is_err() {
                            let _ = interactive_handle.cmd_tx.send(InteractiveCommand::Quit).await;
                            break;
                        }
                    }
                    Ok(InteractiveEvent::Ended { reason }) => {
                        let payload = serde_json::json!({"type": "debug_ended", "reason": reason});
                        let _ = socket::write_json_line(write_half, &payload).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn run_gremlin_task(
    mut gremlin: Gremlin,
    resume_from: Option<&str>,
) -> (i32, Box<dyn crate::artifacts::registry::ArtifactRegistry>) {
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
    (exit_code, gremlin.registry)
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
            if let Some(ref mut f) = file {
                let _ = f.write_all(line.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
                let _ = f.flush().await;
            }
            // Broadcast to live subscribers — ignore errors (no subscribers).
            let _ = broadcast_tx.send(line);
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

/// Result of [`launch_child`]: a state watch receiver and a oneshot for the
/// child's artifact registry (sent after the child reaches a terminal state).
pub(crate) struct LaunchResult {
    pub state_rx: watch::Receiver<RunState>,
    pub registry_rx: tokio::sync::oneshot::Receiver<
        Option<Box<dyn crate::artifacts::registry::ArtifactRegistry>>,
    >,
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
    gremlin.interactive_session = Some(interactive_session);

    // Spawn the log writer.
    let log_path = gremlin.state_dir.join("log");
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

    let (registry_tx, registry_rx) = tokio::sync::oneshot::channel();

    let state_tx_clone = state_tx.clone();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_for_task = aborted.clone();

    // cancel_token is already set on the gremlin (inherited via fork or
    // just created above).  No need to overwrite.

    let id_clone = id.clone();

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, registry) = run_gremlin_task(gremlin, None).await;
        // Send the registry before updating state so the parent can read it.
        let _ = registry_tx.send(Some(registry));
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
    };
    // Insert into RUN_MAP *after* spawning so the JoinHandle is present
    // from the moment the entry exists.
    get_run_map().lock().unwrap().insert(id.clone(), handle);
    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    LaunchResult {
        state_rx,
        registry_rx,
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
