//! Query and state-inspection op handlers: ls, status, info.
//! Also `snapshot` (rewrites definition.json) and `artifact` (reads + the
//! `put` write path), which are query-like in that they inspect terminal
//! gremlins rather than driving lifecycle transitions.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::config;
use crate::definition::{GremlinDefinition, StaticDefinition};
use crate::executor::gremlin::Gremlin;
use crate::executor::state::{self, FileSystemStateStore};
use crate::executor::str_field;

use super::super::run_registry::RunRegistry;
use super::super::state_view::state_view_for_id;
use super::super::{error_response, ok_response, valid_id};

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

pub(crate) async fn handle_ls(_request: &Value, registry: &Arc<RunRegistry>) -> Value {
    let mut rows: Vec<Value> = Vec::new();

    let (ids, pairs) = registry.list_run_stores_with_ids().await;
    let live_ids: std::collections::HashSet<String> = ids.into_iter().collect();
    {
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

pub(crate) async fn handle_status(request: &Value, registry: &Arc<RunRegistry>) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match state_view_for_id(id, registry).await {
        Ok(view) => ok_response(Value::Object(view.common_json())),
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

pub(crate) async fn handle_info(request: &Value, registry: &Arc<RunRegistry>) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let view = match state_view_for_id(id, registry).await {
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

pub(crate) async fn handle_artifact(request: &Value, registry: &Arc<RunRegistry>) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    if request.get("exists").is_some() {
        return handle_artifact_exists(id, request, registry).await;
    }

    if request.get("scope").is_some() {
        return error_response("'scope' requires 'exists'");
    }

    if request.get("put").and_then(|v| v.as_str()).is_some() {
        let _guard = registry.acquire_id_lock(id).await;

        let is_running = registry.contains_run(id).await;
        if is_running {
            return error_response(&format!(
                "gremlin {id} is running — cannot write artifacts to an active gremlin"
            ));
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

    let store = match resolve_store_for_id(id, registry).await {
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

/// Validate one `--exists` URI: a file `artifact://` URI with a safe relative name.
fn valid_exists_uri(uri: &str) -> bool {
    uri.strip_prefix("artifact://").is_some_and(|name| {
        !name.contains(|c: char| c == '\\' || c.is_control())
            && name.split('/').all(|seg| !matches!(seg, "" | "." | ".."))
    })
}

/// Existence-only artifact query: replies with the missing URIs (request
/// order, deduplicated) or an operational error, which takes precedence.
async fn handle_artifact_exists(id: &str, request: &Value, registry: &Arc<RunRegistry>) -> Value {
    for conflicting in ["put", "list", "artifact", "json_path"] {
        if request.get(conflicting).is_some() {
            return error_response(&format!("'exists' conflicts with '{conflicting}'"));
        }
    }
    let Some(items) = request.get("exists").and_then(|v| v.as_array()) else {
        return error_response("'exists' must be an array of artifact URIs");
    };
    if items.is_empty() {
        return error_response("'exists' requires at least one artifact URI");
    }
    let mut uris: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let Some(uri) = item.as_str() else {
            return error_response("'exists' entries must be strings");
        };
        if !valid_exists_uri(uri) {
            return error_response("expected an artifact:// URI with a safe relative name");
        }
        if seen.insert(uri) {
            uris.push(uri.to_string());
        }
    }

    let result = match request.get("scope") {
        None => {
            let store = match resolve_store_for_id(id, registry).await {
                Ok(store) => store,
                Err(resp) => return resp,
            };
            store
                .missing_artifacts(&uris)
                .await
                .map_err(|e| format!("gremlin {id}: {e}"))
        }
        Some(Value::String(token)) => registry.missing_in_scope(id, token, &uris),
        Some(_) => Err("'scope' must be a string".to_string()),
    };
    match result {
        Ok(missing) => ok_response(serde_json::json!({ "id": id, "missing": missing })),
        Err(e) => error_response(&e),
    }
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

pub(crate) async fn handle_snapshot(request: &Value, registry: &Arc<RunRegistry>) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let _guard = registry.acquire_id_lock(id).await;

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

pub(crate) async fn resolve_store_for_id(
    id: &str,
    registry: &Arc<RunRegistry>,
) -> Result<Arc<dyn state::StateStore + Send + Sync>, Value> {
    if let Some(store) = registry.get_run_store(id).await {
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
