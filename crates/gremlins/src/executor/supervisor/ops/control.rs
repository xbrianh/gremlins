//! Lifecycle-mutating op handlers: launch, stop, resume, rm.
//! Also session-scoped stop helpers and runtime artifact key helpers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::config;
use crate::definition::{StageSpec, StaticDefinition};
use crate::executor::event::EventKind;
use crate::executor::gremlin::{Gremlin, GremlinConfig};
use crate::executor::state;

use super::super::actor;
use super::super::actor_client::Client;
use super::super::event::emit_event;
use super::super::lifecycle::spawn_gremlin_runner;
use super::super::lifecycle::terminate_and_reap;
use super::super::{error_response, ok_response, valid_id};

pub(crate) async fn handle_session_stop(
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
    session_id: &str,
) -> Value {
    if let Some(id) = request.get("id").and_then(|v| v.as_str()) {
        // Relinquish ownership, drop the parked gremlin, then cancel the
        // in-flight turn and wait for it to drain.
        let client = Client::new(actor_tx.clone());
        client.session_remove_chat_id(session_id, id).await;
        client.session_take_gremlin(session_id, id).await;
        if let Some(mut done) = client
            .session_cancel_turn(session_id, Some(id), false)
            .await
        {
            let _ = done.wait_for(|v| *v).await;
        }
    }
    handle_stop(request, actor_tx).await
}

/// Remove a chat gremlin from whichever session owns it and cancel its turn.
pub(crate) async fn stop_session_chat_gremlin(
    id: &str,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
) {
    let client = Client::new(actor_tx.clone());
    let live = client.contains_run(id).await;

    for session_id in client.session_get_all().await {
        if !client.session_remove_chat_id(&session_id, id).await {
            continue;
        }
        client.session_take_gremlin(&session_id, id).await;
        let done = client
            .session_cancel_turn(&session_id, Some(id), false)
            .await;
        if live {
            if let Some(mut done) = done {
                let _ = done.wait_for(|v| *v).await;
            }
        }
        return;
    }
}

// ---------------------------------------------------------------------------
// launch
// ---------------------------------------------------------------------------

pub(crate) async fn handle_launch(
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
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

    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));
    let model_override = request
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let gremlin_def = match StaticDefinition::from_json5_file(
        &definition_path,
        model_override,
        default_model.as_deref(),
    ) {
        Ok(d) => d,
        Err(e) => return error_response(&format!("invalid definition: {e}")),
    };

    let resolved_stage_inputs = match &gremlin_def.bootstrap.source {
        Some(source) => resolve_filepath_source_values(source, &stage_inputs, &project_root),
        None => stage_inputs.clone(),
    };

    if let Err(e) = validate_launch_inputs(&gremlin_def, &resolved_stage_inputs, &project_root) {
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
    let gremlin = match Gremlin::init(
        definition_name,
        &definition_path,
        &gremlin_def,
        &resolved_stage_inputs,
        model_override,
        &GremlinConfig {
            ephemeral,
            extra_sandbox_roots: Vec::new(),
        },
    ) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("failed to create gremlin: {e}")),
    };

    {
        let launch_cmd = std::iter::once(definition.to_string())
            .chain(raw_args.iter().cloned())
            .map(|arg| shell_escape(&arg))
            .collect::<Vec<_>>()
            .join(" ");
        let patch = serde_json::json!({
            "metadata": { "cli": { "launch_cmd": launch_cmd } }
        });
        gremlin.state.patch(&[], patch.as_object().unwrap());
    }

    let id = gremlin.id.to_string();

    spawn_gremlin_runner(actor_tx, gremlin, "starting", None, definition_name, true);

    ok_response(serde_json::json!({"id": id}))
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

pub(crate) async fn handle_stop(
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    // The actor owns the run map: it removes the entry, cancels the task,
    // publishes the terminal state and releases the log subscription. A
    // `Some` reply means the id was live and the reap below is still ours.
    let client = Client::new(actor_tx.clone());
    let stopped: Option<actor::StoppedRun> = client.stop_run(id).await;

    if stopped.is_some() {
        terminate_and_reap(id);
        return ok_response(serde_json::json!({"id": id, "status": "stopping"}));
    }

    let state_store = match state::FileSystemStateStore::open(id) {
        Ok(store) => store,
        Err(_) => return error_response(&format!("unknown gremlin {id:?}")),
    };
    let state_dir = state_store.root_dir().to_path_buf();
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

fn load_definition_snapshot(gremlin: &Gremlin) -> Result<StaticDefinition, Value> {
    let mut blob = gremlin
        .state
        .open_blob("definition.json", state::BlobMode::Read)
        .map_err(|e| {
            error_response(&format!(
                "gremlin {}: failed to read definition snapshot: {e}",
                gremlin.id.as_str()
            ))
        })?;
    let mut bytes = Vec::new();
    use std::io::Read;
    blob.read_to_end(&mut bytes).map_err(|e| {
        error_response(&format!(
            "gremlin {}: failed to read definition snapshot: {e}",
            gremlin.id.as_str()
        ))
    })?;
    StaticDefinition::from_expanded_bytes(&bytes, None, None).map_err(|e| {
        error_response(&format!(
            "gremlin {}: invalid definition snapshot: {e}",
            gremlin.id.as_str()
        ))
    })
}

#[derive(Debug, Default)]
pub(crate) struct ParallelRuntimeKeys {
    rerun_groups: HashSet<String>,
    rerun_children: HashSet<String>,
    kept_groups: HashSet<String>,
    kept_children: HashSet<String>,
}

impl ParallelRuntimeKeys {
    pub(crate) fn is_rerun(&self, key: &str) -> bool {
        let Some(rest) = key.strip_prefix("artifact://") else {
            return false;
        };
        if let Some((scope, _child)) = rest.split_once("/done/") {
            let group = scope.rsplit('/').next().unwrap_or(scope);
            if self.rerun_groups.contains(group) && !self.kept_groups.contains(group) {
                return true;
            }
        }
        if let Some((child, original)) = rest.split_once('/') {
            if !original.is_empty()
                && self.rerun_children.contains(child)
                && !self.kept_children.contains(child)
            {
                return true;
            }
        }
        false
    }
}

pub(crate) fn parallel_runtime_keys(stages: &[StageSpec], target: &str) -> ParallelRuntimeKeys {
    let mut keys = ParallelRuntimeKeys::default();
    let mut found_target = false;
    for stage in stages {
        if stage.name() == target {
            found_target = true;
        }
        collect_parallel_runtime_keys(stage, found_target, &mut keys);
    }
    keys
}

fn collect_parallel_runtime_keys(stage: &StageSpec, rerun: bool, keys: &mut ParallelRuntimeKeys) {
    match stage {
        StageSpec::Parallel { attrs, body, .. } => {
            for child in body {
                let child_name = child.name().to_string();
                if rerun {
                    keys.rerun_groups.insert(attrs.name.clone());
                    keys.rerun_children.insert(child_name);
                } else {
                    keys.kept_groups.insert(attrs.name.clone());
                    keys.kept_children.insert(child_name);
                }
            }
            for child in body {
                collect_parallel_runtime_keys(child, rerun, keys);
            }
        }
        StageSpec::Sequence { body, .. } => {
            for child in body {
                collect_parallel_runtime_keys(child, rerun, keys);
            }
        }
        StageSpec::Agent { .. } | StageSpec::Exec { .. } => {}
    }
}

pub(crate) async fn handle_resume(
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let client = Client::new(actor_tx.clone());
    let _guard = client.acquire_id_lock(id).await;

    if client.contains_run(id).await {
        return error_response(&format!("gremlin {id} is already running"));
    }

    let mut gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let status = gremlin.state.read_str("status");
    if status == "running" {
        return error_response(&format!("gremlin {id} is already running — use stop first"));
    }

    let requested_stage = request
        .get("stage")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let resume_with_stage = requested_stage.is_some();

    if status == "done" && !resume_with_stage {
        return error_response(&format!("gremlin {id} is already done"));
    }

    let has_bail = gremlin.state.stage_error().is_some();
    if status != "stopped" && !has_bail && !(status == "done" && resume_with_stage) {
        return error_response(&format!(
            "gremlin {id} cannot be resumed — status is {status:?}"
        ));
    }

    let resume_stage = if let Some(target) = requested_stage {
        let definition = match load_definition_snapshot(&gremlin) {
            Ok(d) => d,
            Err(resp) => return resp,
        };
        if !definition.has_top_level_stage(&target) {
            return error_response(&format!("gremlin {id}: unknown top-level stage {target:?}"));
        }

        let produced_before = match definition.artifact_keys_produced_before(&target) {
            Ok(keys) => keys,
            Err(e) => return error_response(&format!("gremlin {id}: {e}")),
        };
        let produced_all = definition.artifact_keys_produced();
        let parallel_keys = parallel_runtime_keys(&definition.stages, &target);
        let registered = gremlin.state.keys().await;
        let mut preserve: HashSet<String> = produced_before;
        for key in registered {
            if produced_all.contains(&key) {
                continue;
            }
            if parallel_keys.is_rerun(&key) {
                continue;
            }
            preserve.insert(key);
        }
        let mut keys: Vec<String> = preserve.into_iter().collect();
        keys.sort();

        let mut rewind_state = gremlin.state.state_tree();
        rewind_state.insert("stage".into(), Value::String(target.clone()));
        rewind_state.insert("status".into(), Value::Null);
        rewind_state.insert("pid".into(), Value::Null);
        rewind_state.insert("exit_code".into(), Value::Null);
        rewind_state.insert("ended_at".into(), Value::Null);
        rewind_state.insert("subprocess_cost_usd".into(), Value::Null);
        rewind_state.remove("parallel_attempts");

        let forked = match gremlin
            .state
            .store_ref()
            .fork_to_stage(&keys, &rewind_state)
            .await
        {
            Ok(forked) => forked,
            Err(e) => return error_response(&format!("gremlin {id}: failed to fork state: {e}")),
        };
        gremlin.state.swap_store(forked);

        target
    } else {
        let stage = gremlin.state.read_str("stage");
        if stage.is_empty() || stage == "starting" {
            return error_response(&format!(
                "gremlin {id} has no recorded stage to resume from"
            ));
        }

        let state_dir = gremlin.state.state_dir();
        let finished = state_dir.join("finished");
        let _ = std::fs::remove_file(&finished);
        stage
    };

    let current_attempt = {
        let a = gremlin.state.read_str("attempt");
        if a.is_empty() {
            "1".to_string()
        } else {
            a
        }
    };
    let new_attempt = format!("{current_attempt}-resume-{}", state::token_hex(2));
    let fields = serde_json::json!({
        "status": "running",
        "ended_at": null,
        "exit_code": null,
        "attempt": new_attempt,
    });
    gremlin.state.patch(&[], fields.as_object().unwrap());

    let definition_name = gremlin.state.read_str("kind");

    spawn_gremlin_runner(
        actor_tx,
        gremlin,
        &resume_stage,
        Some(&resume_stage),
        &definition_name,
        true,
    );

    ok_response(serde_json::json!({ "id": id, "resume_from": resume_stage }))
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

pub(crate) async fn handle_rm(
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::super::actor::ActorMsg>,
) -> Value {
    let id = match valid_id(request) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let purge = request
        .get("purge")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let client = Client::new(actor_tx.clone());
    let _guard = client.acquire_id_lock(id).await;

    if client.contains_run(id).await {
        return error_response(&format!(
            "gremlin {id} is running — use `gremlins stop {id}` first"
        ));
    }

    let gremlin = match Gremlin::from(id) {
        Ok(g) => g,
        Err(e) => return error_response(&format!("gremlin {id}: {e}")),
    };

    let mut gremlin = gremlin;
    if let Err(e) = gremlin.attach_clean_cmds() {
        log::warn!("gremlin {id}: could not load definition for clean commands: {e}");
    }

    gremlin.clean(purge).await;

    emit_event(EventKind::RunRemoved { id: id.to_string() });

    ok_response(serde_json::json!({"id": id, "removed": true}))
}

pub(crate) fn shell_escape(arg: &str) -> String {
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

pub(crate) fn parse_stage_inputs(raw: &[String]) -> Result<HashMap<String, String>, String> {
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

pub(crate) fn resolve_filepath_source_values(
    source: &crate::schemas::bootstrap::InputSources,
    stage_inputs: &HashMap<String, String>,
    project_root: &Path,
) -> HashMap<String, String> {
    let mut resolved = stage_inputs.clone();
    for (key, src) in &source.sources {
        if !src.types.iter().any(|kind| kind == "filepath") {
            continue;
        }
        let Some(value) = stage_inputs.get(key) else {
            continue;
        };
        if value.is_empty() || Path::new(value).is_absolute() {
            continue;
        }
        let candidate = project_root.join(value);
        if candidate.is_file() {
            resolved.insert(key.clone(), candidate.to_string_lossy().into_owned());
        }
    }
    resolved
}

pub(crate) fn validate_launch_inputs(
    gremlin_def: &crate::definition::StaticDefinition,
    stage_inputs: &HashMap<String, String>,
    project_root: &Path,
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
            bootstrap::validate_source_values_under(source, stage_inputs, project_root)
                .map_err(|e| format!("{e}"))?;
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
