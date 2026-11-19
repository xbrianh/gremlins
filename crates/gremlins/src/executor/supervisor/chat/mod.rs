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
use crate::clients::interactive::InteractiveCommand;
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
    /// The opaque execution ID published at turn start. The attachment
    /// handler uses this to bind observation to one execution, not the
    /// resettable run-id/nonce pair.
    pub(crate) execution_id: Option<String>,
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

pub(crate) async fn handle_chat(
    request: &Value,
    writer: &SharedWriter,
    registry: &Arc<RunRegistry>,
    session_task: &SessionTaskHandle,
) {
    log::debug!("handle_chat: received chat request");

    // The daemon owns the conversation; the session owns the gremlin. The
    // front-end never sends a gremlin id — it only addresses the session.
    let text = request.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        let resp = error_response("missing 'text' field");
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
    // through the run's InteractiveHandle directly — never through the
    // session's persistent chat-gremlin task queue.
    if let Some(target_obj) = request.get("target").and_then(|v| v.as_object()) {
        let target_id = target_obj.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let target_nonce = target_obj
            .get("nonce")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if target_id.is_empty() {
            let resp = error_response("target.id is required");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
        // Nonces start at 1; 0 means the field was missing or the client
        // is holding a stale (unknown-run) nonce from a failed lookup.
        if target_nonce == 0 {
            let resp = error_response("target.nonce is required");
            let _ = writer.write_json_line(&resp).await;
            return;
        }

        // ── Session ownership check ───────────────────────────────────
        // The session socket provides transport isolation, but also verify
        // that a chat gremlin target belongs to THIS session — not a
        // different session's home chat. Pipeline (non-chat) runs are
        // global and pass through; the nonce guards against stale targets.
        {
            let chat_ids = session_task.get_chat_ids().await;
            if !chat_ids.is_empty() && !chat_ids.iter().any(|id| id == target_id) {
                // The target is not one of our chat gremlins. If the
                // registry store records `kind: "chat"`, this is another
                // session's chat gremlin — reject.
                if let Some(store) = registry.get_run_store(target_id).await {
                    if store.read_str("kind") == "chat" {
                        let resp = error_response(&format!(
                            "target {}: not owned by this session",
                            target_id
                        ));
                        let _ = writer.write_json_line(&resp).await;
                        return;
                    }
                }
                // Not a chat gremlin (or not in the registry at all —
                // the resolve below will surface that). Allow pipeline
                // runs through.
            }
        }

        // Resolve the interactive handle. `attach()` validates the nonce
        // atomically, so we only need a single coarse liveness gate here.
        let Some((interactive, _, _)) = registry.get_run_interactive(target_id).await else {
            let resp = error_response(&format!("gremlin {} is not running", target_id));
            let _ = writer.write_json_line(&resp).await;
            return;
        };

        let debug_target = interactive.target();
        let execution_id = debug_target.execution_id().map(|s| s.to_string());
        if debug_target.nonce() != target_nonce {
            let resp = error_response("execution has been replaced; target is stale");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
        // Validate the client-supplied execution ID. The authoritative
        // descriptor is the one the turn announced at start
        // (`CurrentTurn.execution_id`), not a value sampled from the run
        // map after enqueue.
        //
        // Chat turns: reject if `CurrentTurn` hasn't published its
        // execution_id yet — the live-target snapshot would describe the
        // *previous* execution between `prepare_next` and the
        // `CurrentTurn` update. Pipeline runs (no matching turn) fall
        // back to the live target's execution_id.
        let current_turn = session_task.get_current_turn().await;
        let is_chat_turn = current_turn.as_ref().is_some_and(|cur| cur.id == target_id);
        let announced_execution_id = current_turn
            .as_ref()
            .filter(|cur| cur.id == target_id)
            .and_then(|cur| cur.execution_id.clone());
        let bound_execution_id = if is_chat_turn {
            // For our own chat turn, only the announced descriptor is
            // authoritative. Don't fall back to the live target.
            announced_execution_id
        } else {
            // Pipeline run or cross-session target: use the live
            // target's execution_id as the binding descriptor.
            announced_execution_id.or_else(|| execution_id.clone())
        };
        let client_execution_id = target_obj.get("execution_id").and_then(|v| v.as_str());
        if let Some(client_eid) = client_execution_id {
            match bound_execution_id.as_deref() {
                Some(actual) if actual != client_eid => {
                    let resp = error_response("execution_id mismatch: target has been replaced");
                    let _ = writer.write_json_line(&resp).await;
                    return;
                }
                None => {
                    // No execution ID published yet for this
                    // execution. For a chat turn this means
                    // `TurnStarted` hasn't been emitted; for a
                    // pipeline run it means the interactive slot
                    // hasn't been minted. Reject.
                    let resp = error_response("execution not yet started; retry after TurnStarted");
                    let _ = writer.write_json_line(&resp).await;
                    return;
                }
                _ => {}
            }
        } else {
            // The plan requires rejecting missing execution_id — a
            // client must identify the target execution.
            let resp = error_response("execution_id required for targeted correction");
            let _ = writer.write_json_line(&resp).await;
            return;
        }

        // `attach()` checks the nonce under the slot lock, closing the
        // TOCTOU window between the coarse check above and actual
        // attachment.
        let attachment = match debug_target.attach() {
            Ok(a) => a,
            Err(err) => {
                let msg = match err {
                    crate::clients::interactive::AttachError::NoLongerActive => {
                        "agent stage is no longer active"
                    }
                    crate::clients::interactive::AttachError::Stale => {
                        "execution has been replaced; target is stale"
                    }
                    crate::clients::interactive::AttachError::Busy => "debugger already attached",
                };
                let resp = error_response(msg);
                let _ = writer.write_json_line(&resp).await;
                return;
            }
        };

        // Spawn a task that holds the attachment, waits for Ready, and
        // delivers the message. The session dispatcher remains responsive
        // and does not wait for the agent to process the command.
        let correction_text = text.to_string();
        let target_id_for_log = target_id.to_string();
        let req_id = request
            .get("req_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let writer_for_error = writer.clone();
        let execution_id_for_task = execution_id.clone();
        tokio::spawn(async move {
            // Wait for the agent to accept the pause.
            let mut attachment = attachment;
            if (&mut attachment.ready).await.is_err() {
                log::warn!(
                    "correction for {}: agent exited before accepting pause",
                    target_id_for_log
                );
                let mut payload = serde_json::json!({
                    "type": "correction_dropped",
                    "id": target_id_for_log,
                    "reason": "agent exited before accepting pause",
                });
                if let Some(ref eid) = execution_id_for_task {
                    payload["execution_id"] = serde_json::Value::String(eid.clone());
                }
                let _ = writer_for_error.write_json_line(&payload).await;
                return; // Attachment drop resets the pause slot.
            }
            // Deliver the correction without amending the system prompt.
            let (delivery_tx, delivery_rx) = tokio::sync::oneshot::channel();
            if let Err(e) = attachment.try_send(InteractiveCommand::Message(
                correction_text.clone(),
                req_id.clone(),
                Some(delivery_tx),
            )) {
                log::warn!("correction for {}: send failed ({})", target_id_for_log, e);
                // Notify the owner that the correction was dropped.
                let mut payload = serde_json::json!({
                    "type": "correction_dropped",
                    "id": target_id_for_log,
                    "reason": format!("command channel closed: {}", e),
                });
                if let Some(ref eid) = execution_id_for_task {
                    payload["execution_id"] = serde_json::Value::String(eid.clone());
                }
                let _ = writer_for_error.write_json_line(&payload).await;
                return;
            }
            // Await delivery confirmation from the agent loop.
            match delivery_rx.await {
                Ok(()) => {
                    let mut payload = serde_json::json!({
                        "type": "delivered",
                        "id": target_id_for_log,
                        "nonce": target_nonce,
                        "req_id": req_id,
                    });
                    if let Some(ref eid) = execution_id_for_task {
                        payload["execution_id"] = serde_json::Value::String(eid.clone());
                    }
                    let _ = writer_for_error.write_json_line(&payload).await;
                }
                Err(_) => {
                    let mut payload = serde_json::json!({
                        "type": "correction_dropped",
                        "id": target_id_for_log,
                        "reason": "agent exited before incorporation",
                    });
                    if let Some(ref eid) = execution_id_for_task {
                        payload["execution_id"] = serde_json::Value::String(eid.clone());
                    }
                    let _ = writer_for_error.write_json_line(&payload).await;
                }
            }
            // Let the attachment drop naturally — it resets the pause slot
            // for the next debugger. The agent has already consumed the
            // command from its inbox.
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
    //    Deliberately no `nonce` / `execution_id` here: the turn has not
    //    started yet (the prompt was only just queued), so any identity
    //    sampled from the run map now describes the *previous* execution.
    //    The turn that starts publishes its execution identity on
    //    `CurrentTurn.execution_id` (see `chat/turn.rs::run_turn`), and
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
async fn resolve_chat_handle(
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
    let id = registry.reserve_chat_id().await;
    let gremlin = create_chat_gremlin(&id, chat_model, system_prompt, project_root, env)
        .map_err(|e| error_response(&e))?;
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
        clean_cmds: Vec::new(),
        enclosing_task_models: None,
        enclosing_model: None,
        discovered_skills: None,
        extra_sandbox_roots: Vec::new(),
        native_definition: None,
    })
}
