//! Query and state-inspection op handlers: ls, status, info.
//! Also `snapshot` (rewrites definition.json) and `artifact` (reads + the
//! `put` write path), which are query-like in that they inspect terminal
//! gremlins rather than driving lifecycle transitions.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::config;
use crate::definition::{GremlinDefinition, StaticDefinition};
use crate::executor::gremlin::Gremlin;
use crate::executor::state::{self, FileSystemStateStore};
use crate::executor::str_field;

use super::super::state_view::state_view_for_id;
use super::super::{error_response, get_id_lock, get_run_map, ok_response, valid_id};

// ---------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------

fn extract_token_cost_fields(tree: &Map<String, Value>) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    if let Some(tu) = tree.get("token_usage").and_then(|v| v.as_object()) {
        if let Some(v) = tu.get("prompt_tokens").and_then(|v| v.as_u64()) {
            fields.insert("prompt_tokens".into(), v.into());
        }
        if let Some(v) = tu.get("completion_tokens").and_then(|v| v.as_u64()) {
            fields.insert("completion_tokens".into(), v.into());
        }
        if let Some(v) = tu.get("reasoning_tokens").and_then(|v| v.as_u64()) {
            fields.insert("reasoning_tokens".into(), v.into());
        }
        if let Some(v) = tu.get("cached_input_tokens").and_then(|v| v.as_u64()) {
            fields.insert("cached_input_tokens".into(), v.into());
        }
        if let Some(v) = tu
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            fields.insert("cache_creation_input_tokens".into(), v.into());
        }
        if let Some(v) = tu.get("turns").and_then(|v| v.as_u64()) {
            fields.insert("turns".into(), v.into());
        }
    }
    if let Some(v) = tree.get("total_cost_usd").and_then(|v| v.as_f64()) {
        fields.insert("cost_usd".into(), v.into());
    }
    fields
}

pub(crate) async fn handle_ls(_request: &Value, _state_root: &Path) -> Value {
    let mut rows: Vec<Value> = Vec::new();

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
            let stage = str_field(&tree, "stage");
            let started_at = str_field(&tree, "started_at");
            let project = str_field(&tree, "project_root");
            let launch = tree
                .get("metadata")
                .and_then(|v| v.get("cli"))
                .and_then(|v| v.get("launch_cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let mut row = serde_json::json!({
                "id": id,
                "status": "running",
                "stage": stage,
                "started_at": started_at,
                "project": project,
                "launch": launch,
            });
            if let Some(obj) = row.as_object_mut() {
                obj.extend(extract_token_cost_fields(&tree));
            }
            rows.push(row);
        }
    }

    for (id, state_json_path) in state::list_state_dirs() {
        if live_ids.contains(&id) {
            continue;
        }
        if state_json_path.with_file_name("closed").exists() {
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
        let stage = str_field(&raw, "stage");
        let started_at = str_field(&raw, "started_at");
        let project = str_field(&raw, "project_root");
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

        let mut row = serde_json::json!({
            "id": id,
            "status": display_status,
            "stage": stage,
            "started_at": started_at,
            "project": project,
            "launch": launch,
        });
        if let Some(obj) = row.as_object_mut() {
            obj.extend(extract_token_cost_fields(&raw));
        }
        rows.push(row);
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

pub(crate) async fn handle_status(request: &Value, _state_root: &Path) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match state_view_for_id(id) {
        Ok(view) => ok_response(Value::Object(view.common_json())),
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

pub(crate) async fn handle_info(request: &Value, _state_root: &Path) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let view = match state_view_for_id(id) {
        Ok(view) => view,
        Err(resp) => return resp,
    };
    let mut map = view.common_json();
    map.insert(
        "log_file".to_string(),
        Value::String(view.state_dir.join("log").to_string_lossy().to_string()),
    );
    map.insert("bail_info".to_string(), view.bail_info.clone());
    ok_response(Value::Object(map))
}

// ---------------------------------------------------------------------------
// artifact
// ---------------------------------------------------------------------------

pub(crate) async fn handle_artifact(request: &Value) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    if request.get("put").and_then(|v| v.as_str()).is_some() {
        let id_lock = get_id_lock(id);
        let _guard = id_lock.lock().await;

        {
            let map = get_run_map().lock().unwrap();
            if map.contains_key(id) {
                return error_response(&format!(
                    "gremlin {id} is running — cannot write artifacts to an active gremlin"
                ));
            }
        }

        let store = match state::StateData::open(id).map(|sd| sd.store_handle()) {
            Ok(s) => s,
            Err(e) => return error_response(&format!("gremlin {id}: {e}")),
        };

        let status = store.read_str("status");
        if status != "done" && status != "stopped" {
            return error_response(&format!(
                "gremlin {id}: status is {status:?} — artifact writes require done or stopped"
            ));
        }

        let artifact = request
            .get("artifact")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if artifact.is_empty() {
            return error_response("missing 'artifact' field");
        }
        let content = request.get("put").and_then(|v| v.as_str()).unwrap();
        let uri = match crate::artifacts::uri::Uri::parse(artifact) {
            Ok(uri) => uri,
            Err(e) => return error_response(&format!("invalid artifact URI {artifact:?}: {e}")),
        };
        match store.write_into_registry(&uri, content).await {
            Ok(path) => {
                return ok_response(
                    serde_json::json!({ "id": id, "artifact": artifact, "path": path }),
                )
            }
            Err(e) => {
                return error_response(&format!("failed to write artifact {artifact:?}: {e}"))
            }
        }
    }

    let store = match resolve_store_for_id(id) {
        Ok(store) => store,
        Err(resp) => return resp,
    };

    let list = request
        .get("list")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if list {
        let keys = store.keys().await;
        return ok_response(serde_json::json!({ "id": id, "keys": keys }));
    }

    let artifact = request
        .get("artifact")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if artifact.is_empty() {
        return error_response("missing 'artifact' field");
    }

    let json_path = request.get("json_path").and_then(|v| v.as_str());
    match store.content(artifact, json_path).await {
        Ok(content) => ok_response(serde_json::json!({ "id": id, "content": content })),
        Err(e) => error_response(&format!("artifact {artifact:?}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

pub(crate) async fn handle_snapshot(request: &Value, _state_root: &Path) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let id_lock = get_id_lock(id);
    let _guard = id_lock.lock().await;

    if let Err(e) = config::init_global() {
        return error_response(&format!("config init: {e}"));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let status = gremlin.state.read_str("status");
    if status != "done" && status != "stopped" {
        return error_response(&format!(
            "gremlin {id}: status is {status:?} — snapshot requires done or stopped"
        ));
    }

    let definition_path = match gremlin.definition_path.clone() {
        Some(p) => p,
        None => {
            return error_response(&format!(
                "gremlin {id}: no definition path recorded in state"
            ))
        }
    };

    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));

    let definition = match StaticDefinition::from_json5_file(
        &definition_path,
        gremlin.model_override.as_deref(),
        default_model.as_deref(),
    ) {
        Ok(d) => d,
        Err(e) => {
            return error_response(&format!(
                "gremlin {id}: failed to load definition from {}: {e}",
                definition_path.display()
            ))
        }
    };

    let bytes: Vec<u8> = match definition.serialize() {
        Ok(b) => b,
        Err(e) => {
            return error_response(&format!(
                "gremlin {id}: failed to serialize definition: {e}"
            ))
        }
    };

    let state_dir = gremlin.state.state_dir();
    let dst = state_dir.join("definition.json");
    let tmp = state_dir.join(format!("definition.json.tmp.{}", state::token_hex(4)));
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return error_response(&format!("gremlin {id}: failed to write snapshot: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, &dst) {
        let _ = std::fs::remove_file(&tmp);
        return error_response(&format!("gremlin {id}: failed to commit snapshot: {e}"));
    }

    ok_response(serde_json::json!({
        "id": id,
        "definition_path": definition_path.display().to_string(),
        "bytes": bytes.len(),
    }))
}

pub(crate) fn resolve_store_for_id(
    id: &str,
) -> Result<Arc<dyn state::StateStore + Send + Sync>, Value> {
    let live = {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|handle| handle.store.clone())
    };
    if let Some(store) = live {
        return Ok(store);
    }
    let state_store = match FileSystemStateStore::open(id) {
        Ok(store) => store,
        Err(_) => return Err(error_response(&format!("unknown gremlin {id:?}"))),
    };
    let state_dir = state_store.root_dir().to_path_buf();
    if !state_dir.join("state.json").is_file() {
        return Err(error_response(&format!("unknown gremlin {id:?}")));
    }
    state::StateData::open(id)
        .map(|sd| sd.store_handle())
        .map_err(|e| error_response(&format!("gremlin {id}: {e}")))
}
