//! Chat turn lifecycle: chat request handling, turn execution, cancellation,
//! and chat gremlin lifecycle.
//!
//! A session's chat gremlin is driven by a persistent task
//! ([`turn::spawn_chat_gremlin`]); the session holds a
//! [`ChatGremlinHandle`] and queues one prompt per operator turn. All
//! terminal-path handling lives in [`turn`] alongside the task.
//!
//! All session state lives in the per-session task
//! ([`super::session_task`]) and all run-map state in [`super::run_registry`]; this
//! module reaches them exclusively through [`SessionTaskHandle`] and the
//! [`RunRegistry`]. There is no shared session handle and no `RUN_MAP`
//! lock — the session task serializes every session-scoped transition.

mod payload;
mod turn;
mod turn_runner;

// Re-exported for the supervisor test module, which reaches these leaf
// helpers as `super::chat::{add_usage_stats, usage_stats_from_state}`.
pub(crate) use self::payload::PromptMsg;
pub(crate) use super::chat_handle::ChatGremlinHandle;
#[allow(unused_imports)]
pub(crate) use payload::{add_usage_stats, chat_payload, usage_stats_from_state};
pub(crate) use turn::{handle_cancel_chat, spawn_chat_gremlin};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};

use crate::clients::client::{Client as ModelClient, ModelURI};
use crate::clients::execution_input::Delivery;
use crate::config;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::state;

use super::run_registry::RunRegistry;
use super::session_task::SessionTaskHandle;
use super::{error_response, ok_response};
use gremlins_magic_bus::connection::SharedWriter;
use tokio::sync::watch;

use crate::clients::cancel::CancelToken;

/// Daemon process environment, captured once at startup. Requests without `env`
/// fall back to this snapshot.
static BASE_PROCESS_ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

/// The in-flight chat turn for a connection, if any.
pub(crate) struct CurrentTurn {
    pub(crate) id: String,
    /// Client-generated turn id, used to match a `cancel_chat` to the turn
    /// it targets so a late cancel cannot hit a subsequent turn.
    pub(crate) turn: Option<String>,
    pub(crate) cancel: Arc<CancelToken>,
    /// Set to `true` once the turn has re-inserted its gremlin (or dropped
    /// it). A follow-up `chat` for the same id waits on this so it never
    /// sees the gremlin as "unknown" mid-turn. Waiters clone the receiver;
    /// the value is sticky so a waiter that subscribes after the turn
    /// finished still proceeds immediately.
    pub(crate) done: watch::Receiver<bool>,
    pub(crate) hard: Arc<AtomicBool>,
    /// Announcement metadata retained for lifecycle assertions, never input authority.
    #[cfg(test)]
    pub(crate) stage_run_id: Option<String>,
}

pub(crate) fn base_process_env() -> &'static HashMap<String, String> {
    BASE_PROCESS_ENV.get_or_init(|| std::env::vars().collect())
}

/// Resolve chat gremlin env: TUI-supplied JSON object wins, otherwise daemon baseline.
pub(crate) fn resolve_chat_env(
    request: &Value,
    base_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    match request.get("env") {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => base_env.clone(),
    }
}

fn chat_error(request: &Value, message: &str) -> Value {
    let mut response = error_response(message);
    if let Some(req_id) = request.get("req_id") {
        response["req_id"] = req_id.clone();
    }
    response
}

pub(crate) async fn handle_chat(
    request: &Value,
    writer: &SharedWriter,
    registry: &Arc<RunRegistry>,
    session_task: &SessionTaskHandle,
) {
    log::debug!("handle_chat: received chat request");

    if [
        "id",
        "nonce",
        "stage",
        "stage_id",
        "execution_id",
        "stage_run_id",
    ]
    .iter()
    .any(|key| request.get(key).is_some())
    {
        let _ = writer
            .write_json_line(&chat_error(
                request,
                "top-level stage-run selectors are not supported; use target with id, nonce, and stage_run_id",
            ))
            .await;
        return;
    }
    if let Some(error) = super::attachment::retired_target_error(request) {
        let mut error = error;
        if let Some(req_id) = request.get("req_id") {
            error["req_id"] = req_id.clone();
        }
        let _ = writer.write_json_line(&error).await;
        return;
    }

    // The daemon owns the conversation; the session owns the gremlin. The
    // front-end never sends a gremlin id — it only addresses the session.
    let text = request.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        let resp = chat_error(request, "missing 'text' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    // Optional repository-context snapshot captured by the TUI. Kept separate
    // from `text` and appended to the system prompt as low-trust metadata.
    let context = request
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Client-generated turn id, echoed on every chat event so the TUI can
    // drop stale events from a superseded turn.
    let turn = request
        .get("turn")
        .and_then(|v| v.as_str())
        .map(String::from);
    // The operator's project root, supplied by the TUI. When absent (non-TUI
    // callers), fall back to the daemon's resolved project root — resolved
    // *after* `init_global()` so `paths.project-root` is visible.
    let project_root = request
        .get("project_root")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);

    // The operator's environment, supplied by the TUI as a JSON object. When
    // absent (non-TUI callers), fall back to the daemon's base process env
    // snapshot so commands still run with a sane environment. The map lives
    // only on the in-memory `Gremlin.env` — never persisted to state or logs.
    let env = resolve_chat_env(request, base_process_env());

    // Optional list of skill names to activate for this turn. Replaces the
    // session's in-memory active skill set (never persisted). The TUI always
    // sends the full active set on each chat request, so `/clear` resets it
    // by sending an empty array.
    let requested_skills: Vec<String> = request
        .get("skills")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // ── Targeted message (correction for a live execution) ──────────
    //
    // When `target` is present the operator wants to correct a specific
    // in-flight execution without starting a new turn. The message goes
    // through the exact execution's input inbox — never through the
    // session's persistent chat-gremlin task queue.
    if let Some(target) = request.get("target") {
        let Some(target_obj) = target.as_object() else {
            let _ = writer
                .write_json_line(&chat_error(request, "target must be an object"))
                .await;
            return;
        };
        let target_id = target_obj.get("id").and_then(Value::as_str).unwrap_or("");
        if target_id.is_empty() {
            let _ = writer
                .write_json_line(&chat_error(request, "target.id is required"))
                .await;
            return;
        }
        let target_nonce = target_obj.get("nonce").and_then(Value::as_u64).unwrap_or(0);
        if target_nonce == 0 {
            let _ = writer
                .write_json_line(&chat_error(request, "target.nonce is required"))
                .await;
            return;
        }
        let Some(client_stage_run_id) = target_obj
            .get("stage_run_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned)
        else {
            let _ = writer
                .write_json_line(&chat_error(request, "target.stage_run_id is required"))
                .await;
            return;
        };
        let bound = match super::input::bind(request, registry, Some(session_task)).await {
            Ok(bound) => bound,
            Err(message) => {
                let message = if message == "chat input requires its owning session" {
                    format!("target {target_id}: not owned by this session")
                } else {
                    message
                };
                let _ = writer.write_json_line(&chat_error(request, &message)).await;
                return;
            }
        };
        let req_id = request
            .get("req_id")
            .and_then(Value::as_str)
            .map(String::from);
        let enqueue = bound
            .sender
            .try_send(req_id.clone().unwrap_or_default(), text.to_owned());
        let identity = bound.identity.clone();
        drop(bound);
        let receipt = match enqueue {
            Ok(receipt) => receipt,
            Err(error) => {
                let _ = writer
                    .write_json_line(&chat_error(request, &error.to_string()))
                    .await;
                return;
            }
        };

        // Acknowledge enqueue before any asynchronous receipt notification.
        let response = ok_response(serde_json::json!({
            "id": identity.run_id,
            "status": "queued",
            "nonce": target_nonce,
            "stage_run_id": client_stage_run_id,
            "req_id": req_id,
        }));
        if writer.write_json_line(&response).await.is_err() {
            return;
        }
        let writer = writer.clone();
        tokio::spawn(async move {
            // A confirmed delivery remains authoritative even after execution ends.
            let delivery = tokio::select! {
                _ = writer.closed() => return,
                result = receipt => result.unwrap_or(Delivery::NotDelivered),
            };
            let payload = if delivery == Delivery::Delivered {
                serde_json::json!({
                    "type": "delivered",
                    "id": identity.run_id,
                    "nonce": target_nonce,
                    "stage_run_id": client_stage_run_id,
                    "req_id": req_id,
                })
            } else {
                serde_json::json!({
                    "type": "correction_dropped",
                    "id": identity.run_id,
                    "stage_run_id": client_stage_run_id,
                    "req_id": req_id,
                    "reason": "agent exited before incorporation",
                })
            };
            let _ = writer.write_json_line(&payload).await;
        });

        return;
    }

    // ── Concurrent submission check ─────────────────────────────────
    // Fast-path advisory check: reject a second home chat prompt while a
    // turn is still in-flight. The real atomic gate is `SetCurrentTurn`
    // inside `run_turn`, which runs under the session task's single
    // dispatcher — two concurrent submissions cannot both pass that
    // compare-and-swap. This check merely avoids the wasted work of
    // config init / client setup when the session is already busy.
    //
    // Check `is_closed` first so a dead session gets an accurate message
    // rather than the misleading "session busy" error.
    if session_task.is_closed().await {
        let resp = error_response("session closed");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    if !session_task.is_idle_for_chat().await {
        let resp =
            error_response("session busy: a turn is in progress; use target for corrections");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // 1. Init config
    if let Err(e) = config::init_global() {
        let resp = error_response(&format!("config init: {e}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let project_root = project_root.unwrap_or_else(config::project_root);

    let cfg = match config::global_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            let resp = error_response(&format!("config error: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // 2. Resolve the chat model through the TUI model resolver:
    //    tui.json -> tui-model -> default-model. The resolver returns a
    //    concrete spec (profile references already resolved), so only the
    //    `ModelURI` shape check remains before the spec goes into the
    //    per-turn chat definition.
    let chat_model = match config::resolve_tui_chat_model(&cfg) {
        Ok(Some((_source, spec))) => match ModelURI::parse(&spec) {
            Ok(uri) => uri,
            Err(e) => {
                let resp = error_response(&format!("invalid chat model {spec:?}: {e}"));
                let _ = writer.write_json_line(&resp).await;
                return;
            }
        },
        Ok(None) => {
            let resp = error_response("no model configured");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
        Err(e) => {
            let resp = error_response(&format!("invalid chat model: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // 3. Gather per-turn inputs: skills and system prompt.
    let project_root_for_skills = project_root.clone();
    let all_skills = Arc::new(
        tokio::task::spawn_blocking(move || crate::skills::discover(&project_root_for_skills))
            .await
            .unwrap_or_default(),
    );
    let system_prompt =
        crate::executor::supervisor::chat_system_prompt::chat_system_prompt().to_string();

    // 4. Resolve the session's chat gremlin handle — reuse, resume, or fresh.
    //    `reusable_chat_gremlin_id` waits for any in-flight turn to finish, so
    //    model-loaded skills pushed by the running turn are visible before the
    //    turn task snapshots the active set below.
    let handle = match resolve_chat_handle(
        registry,
        session_task,
        &chat_model,
        &system_prompt,
        &project_root,
        &env,
    )
    .await
    {
        Ok(handle) => handle,
        Err(resp) => {
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    if session_task.is_closed().await {
        let resp = error_response("session closed");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // 5. Queue the turn onto the persistent task. The task takes the gremlin
    //    out of the session, installs a fresh per-turn cancel token, runs the
    //    stage, and re-parks it. A `false` reply means the task has exited —
    //    drop the stale handle and report the failure.
    let prompt = PromptMsg {
        text: text.to_string(),
        context: context.to_string(),
        turn: turn.clone(),
        chat_model: chat_model.clone(),
        system_prompt,
        project_root,
        env,
        requested_skills,
        all_skills,
        writer: writer.clone(),
    };
    if !handle.send(prompt).await {
        session_task.set_chat_handle(None).await;
        let resp = error_response("chat gremlin task exited");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // 6. Acknowledge the request so the TUI's `send_request` completes. Chat
    //    events themselves are unsolicited lines written by the task above and
    //    classified separately by the client read loop.
    //
    //    Deliberately no `nonce` / `stage_run_id` here: the turn has not
    //    started yet (the prompt was only just queued), so any identity
    //    sampled from the run map now describes the *previous* execution.
    //    The turn that starts publishes its stage run identity on
    //    `CurrentTurn.stage_run_id` (see `chat/turn.rs::run_turn`), and
    //    attachments bind to that descriptor; corrections validate against it
    //    below.
    let ack = serde_json::json!({"id": handle.id, "status": "streaming"});
    let _ = writer.write_json_line(&ok_response(ack)).await;
}

/// Resolve the session's chat gremlin handle — reuse, resume, or fresh.
///
/// The decision is a flat early-return chain:
/// 1. If the session holds a live handle whose task is running and whose id
///    matches a parked gremlin, return it.
/// 2. If a parked gremlin id exists but the handle is stale (task exited),
///    re-drive the parked gremlin under the same id.
/// 3. Otherwise reserve a fresh id and spawn a brand-new chat gremlin task.
///
/// On failure returns the already-formatted error response (the caller
/// is responsible for writing it).
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_chat_handle(
    registry: &Arc<RunRegistry>,
    session_task: &SessionTaskHandle,
    chat_model: &ModelURI,
    system_prompt: &str,
    project_root: &Path,
    env: &HashMap<String, String>,
) -> Result<ChatGremlinHandle, Value> {
    // Run-map reads (`contains_run`, `reserve_chat_id`) go through the
    // registry; every session-scoped operation uses `session_task`.
    let id = reusable_chat_gremlin_id(session_task).await;

    if let Some(existing_id) = &id {
        // The gremlin may have been removed from the run map by a control-side
        // `stop`. Treat it as gone and let the next message start fresh.
        if !registry.contains_run(existing_id).await {
            let _ = session_task.take_gremlin(existing_id).await;
            let _ = session_task.remove_chat_id(existing_id).await;
            session_task.set_chat_handle(None).await;
        } else {
            // Live handle with matching id: reuse.
            if let Some(handle) = session_task.get_chat_handle().await {
                if !handle.is_finished() && handle.id == *existing_id {
                    return Ok(handle);
                }
            }
            // Stale handle (task exited): re-drive the parked gremlin.
            session_task.set_chat_handle(None).await;
            if let Some(handle) =
                spawn_resumed_chat_gremlin(existing_id, session_task, registry).await
            {
                return Ok(handle);
            }
            // Parked gremlin is gone — remove stale ownership.
            let _ = session_task.remove_chat_id(existing_id).await;
        }
    }

    // Fresh gremlin.
    spawn_fresh_chat_gremlin(
        session_task,
        registry,
        chat_model,
        system_prompt,
        project_root,
        env,
    )
    .await
}

/// Re-drive a parked chat gremlin whose task has exited.
///
/// A permanent cancel ends a task but leaves the gremlin parked (the turn's
/// terminal path re-inserts it before the loop breaks). Re-driving it under the
/// same id keeps its run-map entry owned and avoids an orphan. Returns `None`
/// when no gremlin is parked for `id` (the caller then builds a fresh one).
async fn spawn_resumed_chat_gremlin(
    id: &str,
    session_task: &SessionTaskHandle,
    registry: &Arc<RunRegistry>,
) -> Option<ChatGremlinHandle> {
    let gremlin = session_task.take_gremlin(id).await?;
    // `spawn_chat_gremlin` re-registers the gremlin (overwriting the stale
    // run-map entry) and stores a fresh handle on the session.
    spawn_chat_gremlin(gremlin, Arc::clone(registry), session_task.clone()).await
}

/// Reserve an id, build a fresh chat gremlin, and spawn its persistent task.
///
/// On success the handle is stored on the session and returned. On failure the
/// already-formatted error response is returned for the caller to write.
#[allow(clippy::too_many_arguments)]
async fn spawn_fresh_chat_gremlin(
    session_task: &SessionTaskHandle,
    registry: &Arc<RunRegistry>,
    chat_model: &ModelURI,
    system_prompt: &str,
    project_root: &Path,
    env: &HashMap<String, String>,
) -> Result<ChatGremlinHandle, Value> {
    let home_run_id = session_task.get_home_run_id().await;
    let id = match &home_run_id {
        Some(id) => id.clone(),
        None => registry.reserve_chat_id().await,
    };
    // Parking commits the reservation atomically; every setup failure leaves
    // the advertised identity owned by the session until retry or teardown.
    let gremlin = create_chat_gremlin(&id, chat_model, system_prompt, project_root, env)
        .map_err(|error| error_response(&error))?;
    if session_task.is_closed().await {
        return Err(error_response("session closed"));
    }
    let handle = spawn_chat_gremlin(gremlin, Arc::clone(registry), session_task.clone())
        .await
        .ok_or_else(|| error_response("session closed"))?;
    // Ownership is registered by `spawn_chat_gremlin` as part of parking.
    Ok(handle)
}

/// Return the session's parked chat gremlin id, waiting for an in-flight
/// turn to re-insert.
pub(crate) async fn reusable_chat_gremlin_id(session_task: &SessionTaskHandle) -> Option<String> {
    loop {
        // Parked gremlin wins; otherwise look at the in-flight turn.
        let (ids, turn) = tokio::join!(
            session_task.get_chat_gremlin_keys(),
            session_task.get_current_turn(),
        );

        if let Some(id) = ids.into_iter().next() {
            return Some(id);
        }

        let done = match turn {
            Some(turn) if !*turn.done.borrow() => Some(turn.done),
            // A turn with `done == true` completed but left no parked gremlin
            // (e.g. a stop removed it after a soft interrupt). Waiting on it
            // would spin forever, so report "no reusable gremlin".
            Some(_) => {
                // The turn finished but its done watch may have fired
                // before the gremlin was re-parked. Recheck.
                let ids = session_task.get_chat_gremlin_keys().await;
                return ids.into_iter().next();
            }
            None => {
                // A gremlin may have been parked between the two lookups.
                // Recheck before giving up.
                let ids = session_task.get_chat_gremlin_keys().await;
                return ids.into_iter().next();
            }
        };

        let mut done = done.expect("in-flight turn checked above");
        if done.wait_for(|v| *v).await.is_err() {
            // Sender dropped without publishing. Re-check the session: the
            // turn may have been cleared by another path and re-inserted the
            // gremlin in the meantime.
            return session_task
                .get_chat_gremlin_keys()
                .await
                .into_iter()
                .next();
        }
        // The turn finished — loop to re-check the parked set.
    }
}

pub(crate) fn create_chat_gremlin(
    id: &str,
    default_model: &ModelURI,
    system_prompt: &str,
    project_root: &Path,
    env: &HashMap<String, String>,
) -> Result<Gremlin, String> {
    use crate::executor::gremlin::RuntimeConfig;
    use crate::executor::state::{BlobMode, StateData};

    let project_root = project_root.to_path_buf();

    // Id is pre-reserved through the actor, so no collision check needed.
    let gremlin_id = validate_gremlin_id(id).map_err(|e| format!("invalid id: {e}"))?;

    // Build initial state and create StateData (which writes state.json).
    let initial: Map<String, Value> = serde_json::json!({
        "id": gremlin_id.as_str(),
        "kind": "chat",
        "project_root": project_root.to_string_lossy(),
        "workdir": "",
        "status": "running",
        "started_at": state::now_stamp(),
        "model": default_model.to_string(),
        "stage": "chat",
        "pid": std::process::id(),
        "attempt": "",
        "exit_code": null,
        "definition_path": "",
        "description": "",
        "parent_id": "",
        "definition_args": [],
        "stage_inputs": {},
        "group_name": "",
        "child_key": "",
        "metadata": {},
    })
    .as_object()
    .unwrap()
    .clone();

    let state = StateData::new(gremlin_id.as_str(), &initial, /* ephemeral */ true)
        .map_err(|e| format!("failed to write state: {e}"))?;

    // Chat definitions are never serialized — skip the definition.json blob.

    // Create empty log file via state blob.
    let _ = state
        .open_blob("log", BlobMode::Write)
        .map_err(|e| format!("failed to create log: {e}"))?;

    let runtime_config = RuntimeConfig::snapshot(None);
    let client = ModelClient::from_uri(default_model.clone())
        .map_err(|e| format!("invalid default model '{default_model}': {e}"))?;

    Ok(Gremlin {
        id: gremlin_id,
        definition_path: None,
        model_override: None,
        definition: Box::new(ChatDefinition::new(
            "chat".to_string(),
            PathBuf::from("."),
            default_model.clone(),
            system_prompt.to_string(),
        )),
        workdir: None,
        project_root,
        state,
        env: env.clone(),
        client,
        loop_iter: "1".to_string(),
        stage_inputs: HashMap::new(),
        runtime_config,
        cancel_token: None,
        interactive_sessions: None,
        execution_inputs: Default::default(),
        clean_cmds: Vec::new(),
        enclosing_task_models: None,
        enclosing_model: None,
        discovered_skills: None,
        extra_sandbox_roots: Vec::new(),
        native_definition: None,
    })
}
