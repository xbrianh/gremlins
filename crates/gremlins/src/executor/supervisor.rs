//! Single-instance executor supervisor.
//!
//! The supervisor owns the run map and the accept loop. It holds a
//! `HashMap<String, RunHandle>` and dispatches socket requests to the
//! appropriate handler.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::sync::{watch, Mutex};

use crate::config;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::socket;
use crate::executor::state;

// ---------------------------------------------------------------------------
// RunHandle
// ---------------------------------------------------------------------------

pub(crate) struct RunHandle {
    pub cancel: watch::Sender<bool>,
    #[allow(dead_code)]
    pub state_tx: watch::Sender<RunState>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunState {
    pub id: String,
    pub status: String,
    pub stage: String,
    pub started_at: String,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// The `_lock` file holds the executor's exclusive advisory flock for the
/// lifetime of the supervisor. When the process exits (after the last
/// gremlin drains), the file descriptor closes and the kernel releases the
/// lock automatically.
pub async fn run_supervisor(listener: UnixListener, state_root: PathBuf, _lock: std::fs::File) {
    let run_map: Arc<Mutex<HashMap<String, RunHandle>>> = Arc::new(Mutex::new(HashMap::new()));
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
                        let run_map = run_map.clone();
                        let state_root = state_root.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, run_map, state_root, shutdown_tx).await;
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
    run_map: Arc<Mutex<HashMap<String, RunHandle>>>,
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

        let response = dispatch_op(&op, &request, &run_map, &state_root, &shutdown_tx).await;

        if let Err(e) = socket::write_json_line(&mut write_half, &response).await {
            log::warn!("supervisor: write error: {e}");
            break;
        }

        if matches!(op.as_str(), "launch" | "stop" | "resume") && run_map.lock().await.is_empty() {
            let _ = shutdown_tx.send(true);
        }
    }
}

// ---------------------------------------------------------------------------
// Op dispatch
// ---------------------------------------------------------------------------

async fn dispatch_op(
    op: &str,
    request: &Value,
    run_map: &Arc<Mutex<HashMap<String, RunHandle>>>,
    state_root: &Path,
    shutdown_tx: &watch::Sender<bool>,
) -> Value {
    match op {
        "launch" => handle_launch(request, run_map, state_root, shutdown_tx).await,
        "stop" => handle_stop(request, run_map).await,
        "resume" => handle_resume(request, run_map, state_root, shutdown_tx).await,
        "ls" => handle_ls(request, run_map, state_root).await,
        "status" => handle_status(request, run_map, state_root).await,
        "info" => handle_info(request, run_map, state_root).await,
        _ => error_response(&format!("unknown op: {op:?}")),
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
    run_map: &Arc<Mutex<HashMap<String, RunHandle>>>,
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

    let gremlin = match Gremlin::init(
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

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.clone(),
        status: "running".to_string(),
        stage: "starting".to_string(),
        started_at: state::now_stamp(),
    });

    let run_map_clone = run_map.clone();
    let id_clone = id.clone();
    let state_tx_clone = state_tx.clone();
    let shutdown_tx_clone = shutdown_tx.clone();

    tokio::spawn(async move {
        let result = run_gremlin_task(gremlin, Some(cancel_rx), None).await;
        run_map_clone.lock().await.remove(&id_clone);
        let _ = state_tx_clone.send(RunState {
            id: id_clone.clone(),
            status: if result == 0 { "done" } else { "stopped" }.to_string(),
            stage: String::new(),
            started_at: String::new(),
        });
        if run_map_clone.lock().await.is_empty() {
            let _ = shutdown_tx_clone.send(true);
        }
    });

    let handle = RunHandle {
        cancel: cancel_tx,
        state_tx,
    };
    run_map.lock().await.insert(id.clone(), handle);

    ok_response(serde_json::json!({"id": id}))
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

async fn handle_stop(request: &Value, run_map: &Arc<Mutex<HashMap<String, RunHandle>>>) -> Value {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return error_response("missing 'id' field");
    }

    if validate_gremlin_id(id).is_err() {
        return error_response(&format!("invalid gremlin id {id:?}"));
    }

    let map = run_map.lock().await;
    match map.get(id) {
        Some(handle) => {
            let _ = handle.cancel.send(true);
            ok_response(serde_json::json!({"id": id, "status": "stopping"}))
        }
        None => {
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
                return error_response(&format!(
                    "gremlin {id} is orphaned (state says running but no executor entry)"
                ));
            }
            error_response(&format!("unknown gremlin {id:?}"))
        }
    }
}

// ---------------------------------------------------------------------------
// resume
// ---------------------------------------------------------------------------

async fn handle_resume(
    request: &Value,
    run_map: &Arc<Mutex<HashMap<String, RunHandle>>>,
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
        let map = run_map.lock().await;
        if map.contains_key(id) {
            return error_response(&format!("gremlin {id} is already running"));
        }
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

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.to_string(),
        status: "running".to_string(),
        stage: stage.clone(),
        started_at: state::now_stamp(),
    });

    let run_map_clone = run_map.clone();
    let id_clone = id.to_string();
    let state_tx_clone = state_tx.clone();
    let shutdown_tx_clone = shutdown_tx.clone();

    tokio::spawn(async move {
        let result = run_gremlin_task(gremlin, Some(cancel_rx), Some(&resume_stage)).await;
        run_map_clone.lock().await.remove(&id_clone);
        let _ = state_tx_clone.send(RunState {
            id: id_clone.clone(),
            status: if result == 0 { "done" } else { "stopped" }.to_string(),
            stage: String::new(),
            started_at: String::new(),
        });
        if run_map_clone.lock().await.is_empty() {
            let _ = shutdown_tx_clone.send(true);
        }
    });

    let handle = RunHandle {
        cancel: cancel_tx,
        state_tx,
    };
    run_map.lock().await.insert(id.to_string(), handle);

    ok_response(serde_json::json!({"id": id, "resume_from": resume_stage_for_response}))
}

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

async fn handle_ls(
    _request: &Value,
    run_map: &Arc<Mutex<HashMap<String, RunHandle>>>,
    state_root: &Path,
) -> Value {
    let live: HashMap<String, String> = {
        let map = run_map.lock().await;
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

async fn handle_status(
    request: &Value,
    run_map: &Arc<Mutex<HashMap<String, RunHandle>>>,
    state_root: &Path,
) -> Value {
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

    let is_live = run_map.lock().await.contains_key(id);

    let status = if is_live {
        "running".to_string()
    } else {
        gremlin.state.read_str("status")
    };

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

async fn handle_info(
    request: &Value,
    run_map: &Arc<Mutex<HashMap<String, RunHandle>>>,
    state_root: &Path,
) -> Value {
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

    let is_live = run_map.lock().await.contains_key(id);

    let status = if is_live {
        "running".to_string()
    } else {
        gremlin.state.read_str("status")
    };

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
// Helpers
// ---------------------------------------------------------------------------

async fn run_gremlin_task(
    mut gremlin: Gremlin,
    cancel: Option<watch::Receiver<bool>>,
    resume_from: Option<&str>,
) -> i32 {
    let log_path = gremlin.state_dir.join("log");
    redirect_stdio_to_log(&log_path);

    match gremlin.run(resume_from, cancel).await {
        Ok(ec) => ec,
        Err(e) => {
            log::error!("gremlin {}: {e}", gremlin.id.as_str());
            gremlin.state.write_terminal_state(1);
            1
        }
    }
}

fn redirect_stdio_to_log(log_path: &std::path::Path) {
    // Per-task stdio redirection is not safe in a multi-gremlin address
    // space — dup2 would corrupt other tasks' output. Instead, each
    // gremlin's output is captured via the logging framework and the
    // per-run log file. This stub remains as a no-op; per-gremlin output
    // capture will be implemented with dedicated pipes/files in a
    // follow-up.
    let _ = log_path;
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
