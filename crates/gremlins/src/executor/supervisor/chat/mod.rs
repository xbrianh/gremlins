//! Chat turn lifecycle: chat request handling, turn execution, cancellation,
//! and chat gremlin lifecycle.
//!
//! Terminal-path handling for a turn lives in [`turn_finalizer`].

mod turn_finalizer;

pub(crate) use turn_finalizer::TurnFinalizer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};
use tokio::sync::watch;

use crate::clients::cancel::CancelToken;
use crate::clients::client::{Client, ModelURI};
use crate::clients::interactive::{InteractiveEvent, StreamEventSender};
use crate::clients::protocol::{add_opt, UsageStats};
use crate::config;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::state;

use super::event::{emit_event, EventKind};
use super::lifecycle::setup_gremlin_channels;
use super::lifecycle::terminate_and_reap;
use super::session::SessionHandle;
use super::{
    error_response, get_run_map, ok_response, CurrentTurn, RunHandle, RunState, SharedWriter,
};

/// Daemon process environment, captured once at startup. Requests without `env`
/// fall back to this snapshot.
static BASE_PROCESS_ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

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
    _state_root: &Path,
    writer: &SharedWriter,
    session: &SessionHandle,
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

    // 3. Wait for any in-flight turn to finish so that LoadSkill pushes
    //    from the running turn are visible in the shared active set before
    //    we snapshot it. This avoids a race where a queued turn's system
    //    prompt is built from a stale snapshot.
    //
    //    The session owns at most one chat gremlin today, but
    //    representations keep a map for multiple.
    let mut id = reusable_chat_gremlin_id(session).await;
    if let Some(existing) = &id {
        // The gremlin may have been removed from the run map by a control-side
        // `stop`. Treat it as gone and let the next message start fresh.
        if !get_run_map()
            .lock()
            .unwrap()
            .contains_key(existing.as_str())
        {
            let mut s = session.lock().await;
            s.chat_ids.remove(existing);
            s.chat_gremlins.remove(existing);
            id = None;
        }
    }

    // 4. Update the session's active skill set and build the system prompt.
    //    The supervisor queues the operator's prompt text onto the ChatDefinition
    //    before each turn; the definition constructs the Agent stage on demand
    //    via next_stage().
    //
    //    Skills are discovered from the project root on each turn so newly
    //    added skills take effect without a daemon restart. Discovery runs in
    //    spawn_blocking so filesystem I/O does not stall the async executor.
    let project_root_for_skills = project_root.clone();
    let all_skills =
        tokio::task::spawn_blocking(move || crate::skills::discover(&project_root_for_skills))
            .await
            .unwrap_or_default();
    let (active_skills, active_skills_arc) = {
        let mut s = session.lock().await;
        let skills_arc = Arc::clone(&s.active_skills);
        let prev_operator = std::mem::take(&mut s.operator_skills);
        let mut guard = skills_arc.lock().unwrap();
        // Merge operator toggles into the active set: add names the operator
        // newly requested, remove names the operator explicitly dropped.
        for name in &requested_skills {
            if !guard.contains(name) {
                guard.push(name.clone());
            }
        }
        guard.retain(|name| requested_skills.contains(name) || !prev_operator.contains(name));
        s.operator_skills = requested_skills.clone();
        let names = guard.clone();
        drop(guard);
        (names, skills_arc)
    };
    let (trusted_section, unresolved_skills) =
        crate::skills::build_system_prompt_section(&all_skills, &active_skills);
    let system_prompt = crate::clients::agent::system_prompts::chat_system_prompt().to_string();

    // 5. Get or create the session's chat gremlin (id was resolved in step 3
    //    after waiting for the in-flight turn).

    let id = match id {
        Some(id) => id,
        None => {
            let gremlin =
                match create_chat_gremlin(&chat_model, &system_prompt, &project_root, &env) {
                    Ok(g) => g,
                    Err(e) => {
                        let resp = error_response(&e);
                        let _ = writer.write_json_line(&resp).await;
                        return;
                    }
                };
            let id = gremlin.id.to_string();
            {
                let mut s = session.lock().await;
                if s.closed {
                    drop(gremlin);
                    let resp = error_response("session closed");
                    let _ = writer.write_json_line(&resp).await;
                    return;
                }
                s.chat_ids.insert(id.clone());
            }
            register_chat_gremlin(session, gremlin).await;
            id
        }
    };

    // Ensure the id is still marked as owned by this session (for the reuse
    // path it already is, and this keeps the invariant local).
    {
        let mut s = session.lock().await;
        if s.closed {
            let resp = error_response("session closed");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
        s.chat_ids.insert(id.clone());
    }

    // 6. Take the gremlin out of the session, install this turn's definition,
    //    stream channel, and a fresh per-turn cancel token, then drop the
    //    session lock before running so other ops are not serialized.
    let (stream_tx, stream_rx) = tokio::sync::mpsc::channel::<InteractiveEvent>(256);
    let turn_cancel = CancelToken::new();
    let mut gremlin = {
        let mut s = session.lock().await;
        match s.chat_gremlins.remove(&id) {
            Some(g) => g,
            None => {
                let resp = error_response(&format!("unknown chat gremlin {id:?}"));
                let _ = writer.write_json_line(&resp).await;
                return;
            }
        }
    };
    // Rebuild the parsed model handle for the reused gremlin so a `/model`
    // change takes effect on the next turn rather than only after the chat
    // gremlin is recreated. Build the client before mutating the gremlin so
    // a failure re-inserts it unchanged (old definition, old client).
    let client = match Client::from_uri(chat_model.clone()) {
        Ok(client) => client,
        Err(e) => {
            let mut s = session.lock().await;
            s.chat_gremlins.insert(id.clone(), gremlin);
            let resp = error_response(&format!("invalid chat model '{chat_model}': {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    gremlin.client = client;
    // Inject the dynamic preamble (repo context + skills) on every turn so
    // newly activated skills and refreshed repository snapshots reach the model.
    // The preamble is part of the user turn, not the system prompt, so the
    // static system prompt stays cache-stable.
    let mut text = text.to_string();
    let dynamic_preamble =
        crate::clients::agent::system_prompts::build_chat_preamble(context, &trusted_section);
    if !dynamic_preamble.is_empty() {
        text = format!("{dynamic_preamble}\n---\n{text}");
    }
    // Queue this turn's prompt onto the ChatDefinition so that
    // next_stage() builds the agent stage on demand.
    gremlin
        .definition
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<ChatDefinition>())
        .expect("chat gremlin must carry a ChatDefinition")
        .set_pending(&text, &chat_model, &system_prompt);
    // Keep the state.json `model` field in sync too.
    let state_patch = serde_json::json!({"model": chat_model.to_string()});
    gremlin.state.patch(&[], state_patch.as_object().unwrap());
    gremlin.runtime_config.stream_events = Some(StreamEventSender::Mpsc(stream_tx));
    gremlin.cancel_token = Some(turn_cancel.clone());
    // Refresh the environment on every turn: the chat gremlin is persistent
    // and reused across turns, so a later turn's operator environment must
    // replace the one captured at creation rather than freeze at first contact.
    gremlin.env = env;
    // Wire up skills for the LoadSkill tool.
    gremlin.active_skills = Some(active_skills_arc);
    gremlin.discovered_skills = Some(Arc::new(all_skills));

    // Point the run-map handle's cancel token at this turn so `stop` cancels
    // the in-flight stage rather than a stale persistent token.
    if let Ok(mut map) = get_run_map().lock() {
        if let Some(handle) = map.get_mut(id.as_str()) {
            handle.cancel = turn_cancel.clone();
        }
    }

    let stage = match gremlin.definition.next_stage().await {
        Ok(s) => s,
        Err(e) => {
            // Re-insert the gremlin (it is still owned by this session)
            // and report the error. The id remains in `chat_ids` so
            // session teardown still shuts it down.
            let mut s = session.lock().await;
            s.chat_gremlins.insert(id.clone(), gremlin);
            let resp = error_response(&format!("definition error: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // Invalidate the current turn's per-turn artifact for a fresh turn.
    // After rollback via `goto("chat-0003")`, a stale `conversation/0004.json`
    // from the abandoned branch would otherwise be loaded by `run_agent` and
    // leak discarded messages into the replacement turn's context. During
    // soft-interrupt recovery this artifact is repopulated by
    // `append_chat_conversation_turn` before the agent re-run, so clearing
    // here only affects fresh turns.
    if let Some(turn_num) = crate::executor::run_agent::chat_turn_from_stage_name(stage.name()) {
        crate::executor::run_agent::clear_chat_turn_artifact(&gremlin, turn_num).await;
    }

    // 7. Run the stage in the background. Events are forwarded as
    //    unsolicited JSON lines to this connection while the dispatch
    //    loop stays live for cancel_chat and close_session.
    let (done_tx, done_rx) = watch::channel(false);
    let done_tx = Arc::new(done_tx);
    let soft_cancel = Arc::new(AtomicBool::new(false));
    {
        let mut s = session.lock().await;
        s.current_turn = Some(CurrentTurn {
            id: id.clone(),
            turn: turn.clone(),
            cancel: turn_cancel.clone(),
            soft_cancel: Arc::clone(&soft_cancel),
            done: done_rx,
            done_tx: Arc::clone(&done_tx),
            interrupted: false,
        });
    }

    let writer_for_turn = writer.clone();
    let turn_ctx = TurnCtx {
        id: id.clone(),
        turn: turn.clone(),
        turn_cancel: turn_cancel.clone(),
        soft_cancel: Arc::clone(&soft_cancel),
        session: Arc::clone(session),
        done: done_tx,
    };
    tokio::spawn(async move {
        run_chat_turn(turn_ctx, stage, gremlin, stream_rx, writer_for_turn).await;
    });

    // Acknowledge the request so the TUI's `send_request` completes.
    // Chat events themselves are unsolicited lines written by the task
    // above and classified separately by the client read loop.
    let _ = writer
        .write_json_line(&ok_response(
            serde_json::json!({"id": id, "status": "streaming"}),
        ))
        .await;

    // Surface unresolved skill names as a non-fatal warning so the
    // operator knows a previously-activated skill is no longer available.
    if !unresolved_skills.is_empty() {
        let msg = format!("skill(s) not found: {}", unresolved_skills.join(", "));
        let warning = serde_json::json!({
            "type": "system_warning",
            "message": msg,
            "turn": turn,
        });
        let _ = writer.write_json_line(&warning).await;
    }
}

/// Return the session's parked chat gremlin id, waiting for an in-flight
/// turn to re-insert.
pub(crate) async fn reusable_chat_gremlin_id(session: &SessionHandle) -> Option<String> {
    loop {
        let (id, wait) = {
            let s = session.lock().await;
            if s.closed {
                return None;
            }
            if let Some(id) = s.chat_gremlins.keys().next().cloned() {
                (Some(id), None)
            } else if let Some(cur) = &s.current_turn {
                // The watch channel is sticky: if `done` is already
                // `true` the turn completed but did not re-insert a
                // gremlin (e.g. a stop removed it after a soft
                // interrupt).  Waiting on a completed turn with no
                // parked gremlin would spin forever, so return
                // `None` immediately.
                if *cur.done.borrow() {
                    (None, None)
                } else {
                    (None, Some(cur.done.clone()))
                }
            } else {
                (None, None)
            }
        };
        match (id, wait) {
            (Some(id), _) => return Some(id),
            (None, Some(mut done)) => {
                if done.wait_for(|v| *v).await.is_err() {
                    // Sender dropped without publishing. Re-check the
                    // session under the lock: the turn may have been
                    // cleared by another path and re-inserted the
                    // gremlin in the meantime.
                    let s = session.lock().await;
                    if let Some(id) = s.chat_gremlins.keys().next().cloned() {
                        return Some(id);
                    }
                    return None;
                }
                continue;
            }
            (None, None) => return None,
        }
    }
}

/// Re-insert a finished turn's gremlin, or drop if session no longer owns it.
/// Returns true if re-inserted.
pub(crate) async fn reinsert_chat_gremlin(
    id: &str,
    gremlin: Gremlin,
    session: &SessionHandle,
) -> bool {
    let mut s = session.lock().await;
    if !s.chat_ids.contains(id) {
        drop(gremlin);
        return false;
    }
    // Guard against resurrection: if the gremlin was stopped (via `stop`
    // from any connection), the run-map entry is gone and we must not
    // re-insert.
    if !get_run_map().lock().unwrap().contains_key(id) {
        drop(gremlin);
        return false;
    }
    s.chat_gremlins.insert(id.to_string(), gremlin);
    true
}

/// Read persisted `state.json.token_usage` as [`UsageStats`].
pub(crate) fn usage_stats_from_state(value: Option<&serde_json::Value>) -> UsageStats {
    let obj = value
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    UsageStats {
        prompt_tokens: obj.get("prompt_tokens").and_then(|v| v.as_u64()),
        completion_tokens: obj.get("completion_tokens").and_then(|v| v.as_u64()),
        cached_input_tokens: obj.get("cached_input_tokens").and_then(|v| v.as_u64()),
        cache_creation_input_tokens: obj
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
        reasoning_tokens: obj.get("reasoning_tokens").and_then(|v| v.as_u64()),
        turns: obj.get("turns").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
    }
}

/// Add a per-prompt usage delta to a conversation baseline.
pub(crate) fn add_usage_stats(baseline: &UsageStats, delta: &UsageStats) -> UsageStats {
    UsageStats {
        prompt_tokens: add_opt(baseline.prompt_tokens, delta.prompt_tokens),
        completion_tokens: add_opt(baseline.completion_tokens, delta.completion_tokens),
        cached_input_tokens: add_opt(baseline.cached_input_tokens, delta.cached_input_tokens),
        cache_creation_input_tokens: add_opt(
            baseline.cache_creation_input_tokens,
            delta.cache_creation_input_tokens,
        ),
        reasoning_tokens: add_opt(baseline.reasoning_tokens, delta.reasoning_tokens),
        turns: baseline.turns + delta.turns,
    }
}

/// Build a chat JSON line, tagged with the client's turn id for stale-event filtering.
pub(crate) fn chat_payload(turn: &Option<String>, fields: serde_json::Value) -> serde_json::Value {
    let mut obj = match fields {
        serde_json::Value::Object(m) => m,
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    if let Some(t) = turn {
        obj.insert("turn".to_string(), serde_json::Value::String(t.clone()));
    }
    serde_json::Value::Object(obj)
}

/// Shared context threaded through a chat turn's forwarding task.
pub(crate) struct TurnCtx {
    id: String,
    turn: Option<String>,
    turn_cancel: Arc<CancelToken>,
    soft_cancel: Arc<AtomicBool>,
    session: SessionHandle,
    done: Arc<watch::Sender<bool>>,
}

async fn run_chat_turn(
    ctx: TurnCtx,
    stage: crate::definition::ExecutorStage,
    mut gremlin: Gremlin,
    mut stream_rx: tokio::sync::mpsc::Receiver<InteractiveEvent>,
    writer: SharedWriter,
) {
    // Terminal-path resources live on the finalizer so every exit path
    // re-inserts the gremlin and clears the in-flight turn the same way.
    let finalizer = TurnFinalizer::new(&ctx, writer.clone());

    let id = ctx.id;
    let turn = ctx.turn;
    let turn_cancel = ctx.turn_cancel;
    let soft_cancel = ctx.soft_cancel;

    // Capture the turn number from the stage name so cancel cleanup
    // can always clear the per-turn artifact.
    let chat_turn_num = crate::executor::run_agent::chat_turn_from_stage_name(stage.name());

    // Capture the conversation baseline from the gremlin's persisted state
    // *before* moving it into the stage runner. `run_stage` accumulates the
    // current prompt's usage into the same state after completion, so this
    // must be a pre-turn snapshot. Missing state (the first chat prompt) is
    // treated as zero.
    let baseline_usage = usage_stats_from_state(gremlin.state.read_field("token_usage").as_ref());

    // Snapshot the pre-turn conversation length so the soft-cancel path
    // can split off only messages added during this turn, avoiding
    // duplication of prior-turn history in the per-turn artifact.
    let pre_turn_len = crate::executor::run_agent::read_chat_conversation(
        &gremlin,
        chat_turn_num.map(|n| n.saturating_sub(1)),
    )
    .await
    .len();

    // Capture the user prompt before moving `stage` into the run task, so
    // the soft-cancel path can fall back to it when the model produced no
    // output before cancellation.
    let user_prompt = match &stage {
        crate::definition::ExecutorStage::Agent { stage: agent, .. } => agent.prompts.join("\n"),
        _ => String::new(),
    };

    // Spawn the stage runner so we can select on it alongside stream events.
    let mut run_task = tokio::spawn(async move {
        let result = crate::executor::run::run_stage(&stage, &mut gremlin).await;
        (result, gremlin)
    });

    // Forward stream events until a terminal event, a write error, or a
    // cancellation.
    loop {
        macro_rules! send {
            ($fields:expr) => {
                let payload = chat_payload(&turn, $fields);
                if writer.write_json_line(&payload).await.is_err() {
                    break;
                }
            };
        }
        tokio::select! {
            biased;

            result = stream_rx.recv() => {
                match result {
                    Some(InteractiveEvent::StreamChunk { text }) => {
                        send!(serde_json::json!({"type": "stream_chunk", "text": text}));
                    }
                    Some(InteractiveEvent::ReasoningChunk { text }) => {
                        send!(serde_json::json!({"type": "reasoning_chunk", "text": text}));
                    }
                    Some(InteractiveEvent::ToolOutputChunk { id, name, call, text }) => {
                        send!(serde_json::json!({"type": "tool_output_chunk", "id": id, "name": name, "call": call, "text": text}));
                    }
                    Some(InteractiveEvent::StreamReset) => {
                        send!(serde_json::json!({"type": "stream_reset"}));
                    }
                    Some(InteractiveEvent::ToolResult { id, name, call, output }) => {
                        send!(serde_json::json!({"type": "tool_result", "id": id, "name": name, "call": call, "output": output}));
                    }
                    Some(InteractiveEvent::TurnComplete { turn: tc_turn, text, tool_calls, stats, totals }) => {
                        let total = totals.as_ref().map(|t| add_usage_stats(&baseline_usage, t));
                        send!(serde_json::json!({"type": "turn_complete", "loop_turn": tc_turn, "text": text, "tool_calls": tool_calls, "stats": stats, "total": total}));
                    }
                    Some(InteractiveEvent::Done { text, summary, usage }) => {
                        let usage = usage.as_ref().map(|u| add_usage_stats(&baseline_usage, u));
                        finalizer
                            .stream_terminal(
                                &mut run_task,
                                serde_json::json!({"type": "done", "id": id, "text": text, "summary": summary, "usage": usage}),
                            )
                            .await;
                        return;
                    }
                    Some(InteractiveEvent::Ended { reason }) => {
                        finalizer
                            .stream_terminal(
                                &mut run_task,
                                serde_json::json!({"type": "ended", "id": id, "reason": reason}),
                            )
                            .await;
                        return;
                    }
                    Some(InteractiveEvent::Ready { .. }) => {
                        // Ignore Ready in chat mode.
                    }
                    None => {
                        finalizer
                            .stream_terminal(
                                &mut run_task,
                                serde_json::json!({"type": "error", "id": id, "message": "agent exited before completing"}),
                            )
                            .await;
                        return;
                    }
                }
            }
            _ = turn_cancel.cancelled() => {
                // Soft interrupt: persist partial messages so the
                // conversation artifact preserves what the model emitted
                // before cancellation, then end the turn immediately.
                // The next turn will prepend an interrupt notice via
                // the ChatDefinition's `interrupted` flag.
                if soft_cancel.load(Ordering::Acquire) {
                    match run_task.await {
                        Ok((Err(crate::executor::RunError::Cancelled { messages, .. }), recovered)) => {
                            finalizer
                                .soft_interrupt(
                                    recovered,
                                    messages,
                                    pre_turn_len,
                                    user_prompt.clone(),
                                    chat_turn_num,
                                )
                                .await;
                            return;
                        }
                        Ok((_, recovered)) => {
                            finalizer.reinsert_and_clear(Some(recovered)).await;
                            finalizer
                                .write(serde_json::json!({"type": "ended", "id": id, "reason": "cancelled"}))
                                .await;
                            return;
                        }
                        Err(join_err) => {
                            let message = if join_err.is_panic() {
                                "agent panicked".to_string()
                            } else {
                                format!("agent task cancelled: {join_err}")
                            };
                            finalizer.reinsert_and_clear(None).await;
                            finalizer
                                .write(serde_json::json!({"type": "ended", "id": id, "reason": message}))
                                .await;
                            return;
                        }
                    }
                }

                // Hard cancel (or a soft interrupt that arrived after the
                // turn had already finished): stop forwarding, clear the
                // per-turn artifact so partial messages don't leak into
                // the next turn, re-insert the gremlin, and emit `ended`.
                match run_task.await {
                    Ok((_, gremlin)) => {
                        finalizer.hard_cancel(Some(gremlin), chat_turn_num).await;
                    }
                    Err(e) => {
                        log::warn!("handle_chat: run task panicked for {id}: {e}");
                        finalizer.hard_cancel(None, chat_turn_num).await;
                    }
                }
                return;
            }

            task_result = &mut run_task => {
                // The run task finished without a terminal event on the
                // stream channel. This is the error path: `run_stage`
                // returned Err, the agent panicked, or the task was
                // cancelled. The mpsc sender inside
                // `gremlin.runtime_config.stream_events` stays alive
                // (held by the now-completed JoinHandle's Gremlin),
                // so `stream_rx` would block forever without this branch.
                let (gremlin_for_reinsert, message) = match task_result {
                    Ok((Ok(()), gremlin)) => {
                        (Some(gremlin), "agent completed without terminal event".to_string())
                    }
                    Ok((Err(e), gremlin)) => {
                        (Some(gremlin), format!("agent error: {e}"))
                    }
                    Err(join_err) => {
                        let msg = if join_err.is_panic() {
                            "agent panicked".to_string()
                        } else {
                            format!("agent task cancelled: {join_err}")
                        };
                        (None, msg)
                    }
                };
                // Re-insert the gremlin (if we have one) and clear the
                // in-flight turn. Do NOT call finish() here — run_task
                // was already polled to completion by the select, and
                // awaiting it again is undefined.
                // Restore the pre-turn conversation so partial messages
                // from a soft-interrupt recovery don't leak into the next
                // chat turn.
                finalizer
                    .task_fallback(gremlin_for_reinsert, message, chat_turn_num)
                    .await;
                return;
            }
        }
    }

    // Reached on a write error (client gone). Re-insert the gremlin and
    // signal completion; nothing more can be written to the dead socket.
    finalizer.finish(&mut run_task).await;
    log::debug!("handle_chat: done forwarding for {id}");
}

/// Cancel the in-flight chat turn. Soft cancel persists partial messages;
/// hard cancel clears the artifact immediately. Turn id matching prevents
/// late cancels from hitting subsequent turns.
pub(crate) async fn handle_cancel_chat(
    request: &Value,
    writer: &SharedWriter,
    session: &SessionHandle,
) {
    let turn = request
        .get("turn")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mode = request
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("hard");
    let soft = mode == "soft";
    let s = session.lock().await;
    let matches = match &s.current_turn {
        Some(cur) => turn
            .as_deref()
            .is_none_or(|t| Some(t) == cur.turn.as_deref()),
        None => false,
    };
    if matches {
        // If the turn has already been interrupted (soft-cancel finished),
        // only a hard cancel is meaningful: clean up the artifact and the
        // `interrupted` flag, then emit `ended` so the TUI can retire the
        // turn. A soft cancel on an already-interrupted turn is a no-op.
        if let Some(cur) = &s.current_turn {
            if cur.interrupted {
                if soft {
                    // Already interrupted — soft cancel is a no-op.
                    drop(s);
                    let _ = writer
                        .write_json_line(&ok_response(serde_json::json!({})))
                        .await;
                    return;
                }

                // Hard cancel on an interrupted turn: take the gremlin
                // from the session, clear the partial artifact and the
                // interrupt flag, then re-insert.  Emit `ended` directly
                // since the turn task has already returned.
                let id = cur.id.clone();
                let turn = cur.turn.clone();
                let done_tx = Arc::clone(&cur.done_tx);
                drop(s);

                let gremlin_opt = {
                    let mut s = session.lock().await;
                    s.chat_gremlins.remove(&id)
                };

                if let Some(mut gremlin) = gremlin_opt {
                    let turn_num = gremlin
                        .definition
                        .as_any_mut()
                        .and_then(|a| a.downcast_ref::<ChatDefinition>())
                        .map(|c| c.turn_count());

                    if let Some(turn_num) = turn_num {
                        crate::executor::run_agent::clear_chat_turn_artifact(&gremlin, turn_num)
                            .await;
                    }

                    if let Some(chat) = gremlin
                        .definition
                        .as_any_mut()
                        .and_then(|a| a.downcast_mut::<ChatDefinition>())
                    {
                        chat.clear_interrupted();
                    }

                    let mut s = session.lock().await;
                    s.chat_gremlins.insert(id.clone(), gremlin);
                    s.current_turn = None;
                    drop(s);
                } else {
                    let mut s = session.lock().await;
                    s.current_turn = None;
                    drop(s);
                }

                let _ = done_tx.send(true);

                let payload = chat_payload(
                    &turn,
                    serde_json::json!({"type": "ended", "id": id, "reason": "cancelled"}),
                );
                let _ = writer.write_json_line(&payload).await;
                let _ = writer
                    .write_json_line(&ok_response(serde_json::json!({})))
                    .await;
                return;
            }

            // Normal (non-interrupted) path: cancel the token but leave
            // `current_turn` installed — its `done` watch receiver is what
            // `handle_chat` waits on when the gremlin is temporarily out
            // of `chat_gremlins`. `run_chat_turn::finish` clears
            // `current_turn` once the turn has completed and re-inserted
            // (or dropped) the gremlin.
            let already_cancelled = cur.cancel.is_cancelled();
            if soft {
                // Only honour a soft request when the token has not yet
                // been cancelled — a hard cancel that arrived first
                // must not be overwritten by a late soft request.
                if !already_cancelled {
                    cur.soft_cancel.store(true, Ordering::Release);
                }
            } else {
                cur.soft_cancel.store(false, Ordering::Release);
            }
            if !already_cancelled {
                cur.cancel.cancel();
            }
        }
    }
    let _ = writer
        .write_json_line(&ok_response(serde_json::json!({})))
        .await;
}

/// Shut down a chat gremlin: remove run-map entry, write terminal state,
/// reap client, emit RunStopped.
pub(crate) async fn cleanup_chat_gremlin(id: &str) {
    if validate_gremlin_id(id).is_err() {
        return;
    }

    // Snapshot the cancel token before removing from the run-map
    // so we can stop the in-flight turn.
    let cancel = {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.cancel.clone())
    };

    // Cancel the in-flight turn so the model stops writing.
    if let Some(cancel) = cancel {
        cancel.cancel();
    }

    // Remove from RUN_MAP. If the id wasn't there (already stopped by
    // the operator mid-session), the persistent map entry still needs
    // removing so the gremlin doesn't linger.
    let state_tx = {
        let mut map = get_run_map().lock().unwrap();
        match map.remove(id) {
            Some(handle) => Some(handle.state_tx.clone()),
            None => None,
        }
    };

    if let Some(state_tx) = state_tx {
        let _ = state_tx.send(RunState {
            id: id.to_string(),
            status: "stopped".to_string(),
            stage: String::new(),
            started_at: String::new(),
        });
    }

    terminate_and_reap(id);
}

async fn register_chat_gremlin(session: &SessionHandle, mut gremlin: Gremlin) {
    let id = gremlin.id.to_string();

    let (stage_stream_broadcast, interactive_handle, log_flush) =
        setup_gremlin_channels(&mut gremlin, None);
    let store = gremlin.state.store_handle();

    let cancel_token = CancelToken::new();
    gremlin.cancel_token = Some(cancel_token.clone());

    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.clone(),
        status: "running".to_string(),
        stage: "chat".to_string(),
        started_at: state::now_stamp(),
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: None,
        aborted: Arc::new(AtomicBool::new(false)),
        state_tx,
        stage_stream_broadcast,
        interactive: interactive_handle,
        store,
        log_flush: Some(log_flush),
    };
    get_run_map().lock().unwrap().insert(id.clone(), handle);

    emit_event(EventKind::RunStarted {
        id: id.clone(),
        definition: "chat".to_string(),
        stage: "chat".to_string(),
    });

    session.lock().await.chat_gremlins.insert(id, gremlin);
}

static CHAT_SEQ: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn create_chat_gremlin(
    default_model: &ModelURI,
    system_prompt: &str,
    project_root: &Path,
    env: &HashMap<String, String>,
) -> Result<Gremlin, String> {
    use crate::executor::gremlin::RuntimeConfig;
    use crate::executor::state::{BlobMode, StateData};

    let project_root = project_root.to_path_buf();

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
    let client = Client::from_uri(default_model.clone())
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
        interactive_session: None,
        clean_cmds: Vec::new(),
        enclosing_task_models: None,
        enclosing_model: None,
        discovered_skills: None,
        active_skills: None,
        extra_sandbox_roots: Vec::new(),
    })
}
