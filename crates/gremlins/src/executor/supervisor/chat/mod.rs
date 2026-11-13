//! Chat turn lifecycle: chat request handling, turn execution, cancellation,
//! and chat gremlin lifecycle.
//!
//! Terminal-path handling for a turn lives in [`turn_finalizer`].
//!
//! All session and run-map state lives in [`super::actor`]; this module reaches
//! it exclusively through [`ActorMsg`] messages. There is no shared session
//! handle and no `RUN_MAP` lock — the actor serializes every transition.

mod turn_finalizer;

pub(crate) use turn_finalizer::TurnFinalizer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};
use tokio::sync::{mpsc, watch};

use crate::clients::cancel::CancelToken;
use crate::clients::client::{Client as ModelClient, ModelURI};
use crate::clients::interactive::{InteractiveEvent, StreamEventSender};
use crate::clients::protocol::{add_opt, UsageStats};
use crate::config;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::state;

use super::actor::ActorMsg;
use super::actor_client::Client;
use super::{error_response, ok_response, CurrentTurn, SharedWriter};

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
    writer: &SharedWriter,
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    session_id: &str,
) {
    log::debug!("handle_chat: received chat request");

    let client = Client::new(actor_tx.clone());

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
    let mut id = reusable_chat_gremlin_id(actor_tx, session_id).await;
    if let Some(existing) = &id {
        // The gremlin may have been removed from the run map by a control-side
        // `stop`. Treat it as gone and let the next message start fresh.
        if !client.contains_run(existing).await {
            let _ = client.session_take_gremlin(session_id, existing).await;
            let _ = client.session_remove_chat_id(session_id, existing).await;
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
    // The actor merges the operator toggles into the session's active set and
    // returns the resulting names plus the shared `Arc` the agent loop reads.
    let (active_skills, active_skills_arc) = match client
        .session_update_skills(session_id, requested_skills.clone())
        .await
    {
        Some(skills) => skills,
        None => {
            let resp = error_response("session closed");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    let (trusted_section, unresolved_skills) =
        crate::skills::build_system_prompt_section(&all_skills, &active_skills);
    let system_prompt = crate::clients::agent::system_prompts::chat_system_prompt().to_string();

    // 5. Get or create the session's chat gremlin (id was resolved in step 3
    //    after waiting for the in-flight turn).

    let id = match id {
        Some(id) => id,
        None => {
            let gremlin = match create_chat_gremlin(
                actor_tx,
                &chat_model,
                &system_prompt,
                &project_root,
                &env,
            ) {
                Ok(g) => g,
                Err(e) => {
                    let resp = error_response(&e);
                    let _ = writer.write_json_line(&resp).await;
                    return;
                }
            };
            let id = gremlin.id.to_string();
            if client.session_is_closed(session_id).await {
                drop(gremlin);
                let resp = error_response("session closed");
                let _ = writer.write_json_line(&resp).await;
                return;
            }
            // `register_chat_gremlin` parks the gremlin and installs the
            // run-map entry; ownership is registered as part of that message.
            client.register_chat_gremlin(session_id, gremlin).await;
            id
        }
    };

    // Ensure the id is still marked as owned by this session (for the reuse
    // path it already is, and this keeps the invariant local).
    if client.session_is_closed(session_id).await {
        let resp = error_response("session closed");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    client.session_add_chat_id(session_id, &id).await;

    // 6. Take the gremlin out of the session, install this turn's definition,
    //    stream channel, and a fresh per-turn cancel token. The session is
    //    actor-owned, so other ops are never serialized behind a lock here.
    let (stream_tx, stream_rx) = tokio::sync::mpsc::channel::<InteractiveEvent>(256);
    let turn_cancel = CancelToken::new();
    let mut gremlin = match client.session_take_gremlin(session_id, &id).await {
        Some(g) => g,
        None => {
            let resp = error_response(&format!("unknown chat gremlin {id:?}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    // Rebuild the parsed model handle for the reused gremlin so a `/model`
    // change takes effect on the next turn rather than only after the chat
    // gremlin is recreated. Build the client before mutating the gremlin so
    // a failure re-inserts it unchanged (old definition, old client).
    let llm_client = match ModelClient::from_uri(chat_model.clone()) {
        Ok(client) => client,
        Err(e) => {
            let _ = client
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
            let resp = error_response(&format!("invalid chat model '{chat_model}': {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    gremlin.client = llm_client;
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
    client.set_run_cancel(&id, turn_cancel.clone()).await;

    let stage = match gremlin.definition.next_stage().await {
        Ok(s) => s,
        Err(e) => {
            // Re-insert the gremlin (it is still owned by this session)
            // and report the error. The id remains owned so session
            // teardown still shuts it down.
            let _ = client
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
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
    client
        .session_set_current_turn(
            session_id,
            CurrentTurn {
                id: id.clone(),
                turn: turn.clone(),
                cancel: turn_cancel.clone(),
                soft_cancel: Arc::clone(&soft_cancel),
                done: done_rx,
                interrupted: false,
            },
        )
        .await;

    let writer_for_turn = writer.clone();
    let turn_ctx = TurnCtx {
        id: id.clone(),
        turn: turn.clone(),
        turn_cancel: turn_cancel.clone(),
        soft_cancel: Arc::clone(&soft_cancel),
        actor_tx: actor_tx.clone(),
        session_id: session_id.to_string(),
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
pub(crate) async fn reusable_chat_gremlin_id(
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    session_id: &str,
) -> Option<String> {
    loop {
        // Parked gremlin wins; otherwise look at the in-flight turn.
        let client = Client::new(actor_tx.clone());
        let (ids, turn) = tokio::join!(
            client.session_get_chat_gremlin_keys(session_id),
            client.session_get_current_turn(session_id),
        );

        if let Some(id) = ids.into_iter().next() {
            return Some(id);
        }

        let done = match turn {
            Some(turn) if !*turn.done.borrow() => Some(turn.done),
            // A turn with `done == true` completed but left no parked gremlin
            // (e.g. a stop removed it after a soft interrupt). Waiting on it
            // would spin forever, so report "no reusable gremlin".
            Some(_) => return None,
            None => return None,
        };

        let mut done = done.expect("in-flight turn checked above");
        if done.wait_for(|v| *v).await.is_err() {
            // Sender dropped without publishing. Re-check the session: the
            // turn may have been cleared by another path and re-inserted the
            // gremlin in the meantime.
            return Client::new(actor_tx.clone())
                .session_get_chat_gremlin_keys(session_id)
                .await
                .into_iter()
                .next();
        }
        // The turn finished — loop to re-check the parked set.
    }
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
    /// Mailbox for the supervisor actor — the turn reaches session and
    /// run-map state exclusively through it.
    pub(crate) actor_tx: mpsc::UnboundedSender<ActorMsg>,
    pub(crate) session_id: String,
    pub(crate) done: Arc<watch::Sender<bool>>,
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
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    session_id: &str,
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

    let client = Client::new(actor_tx.clone());
    let snapshot = client.session_get_current_turn(session_id).await;
    let matches = match &snapshot {
        Some(cur) => turn
            .as_deref()
            .is_none_or(|t| Some(t) == cur.turn.as_deref()),
        None => false,
    };
    if !matches {
        let _ = writer
            .write_json_line(&ok_response(serde_json::json!({})))
            .await;
        return;
    }
    let cur = snapshot.expect("matched turn is present");

    // If the turn has already been interrupted (soft-cancel finished),
    // only a hard cancel is meaningful: clean up the artifact and the
    // `interrupted` flag, then emit `ended` so the TUI can retire the
    // turn. A soft cancel on an already-interrupted turn is a no-op.
    if cur.interrupted {
        if soft {
            // Already interrupted — soft cancel is a no-op.
            let _ = writer
                .write_json_line(&ok_response(serde_json::json!({})))
                .await;
            return;
        }

        // Hard cancel on an interrupted turn: take the gremlin from the
        // session, clear the partial artifact and the interrupt flag, then
        // re-insert. Emit `ended` directly since the turn task has already
        // returned.
        let id = cur.id.clone();
        let turn = cur.turn.clone();
        let mut done_rx = cur.done.clone();

        let gremlin_opt = client.session_take_gremlin(session_id, &id).await;

        if let Some(mut gremlin) = gremlin_opt {
            let turn_num = gremlin
                .definition
                .as_any_mut()
                .and_then(|a| a.downcast_ref::<ChatDefinition>())
                .map(|c| c.turn_count());

            if let Some(turn_num) = turn_num {
                crate::executor::run_agent::clear_chat_turn_artifact(&gremlin, turn_num).await;
            }

            if let Some(chat) = gremlin
                .definition
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<ChatDefinition>())
            {
                chat.clear_interrupted();
            }

            let _ = client
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
        }

        // Clear the in-flight turn and publish completion (the cancel token is
        // the turn's identity guard; only the matching turn is cleared).
        client
            .session_clear_current_turn(session_id, Arc::clone(&cur.cancel))
            .await;
        let _ = done_rx.wait_for(|v| *v).await;

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
    let _ = client
        .session_cancel_turn(session_id, Some(&cur.id), soft)
        .await;

    let _ = writer
        .write_json_line(&ok_response(serde_json::json!({})))
        .await;
}

static CHAT_SEQ: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn create_chat_gremlin(
    _actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
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
    let gremlin_id = {
        let seq = CHAT_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("chat-{seq:04x}");
        validate_gremlin_id(&candidate).map_err(|e| format!("invalid id: {e}"))?
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
        interactive_session: None,
        clean_cmds: Vec::new(),
        enclosing_task_models: None,
        enclosing_model: None,
        discovered_skills: None,
        active_skills: None,
        extra_sandbox_roots: Vec::new(),
    })
}
