//! Chat turn execution: the persistent chat-gremlin task, its prompt channel,
//! and per-turn cancellation.
//!
//! A chat session's gremlin is driven by a single long-lived task
//! ([`spawn_chat_gremlin`]). The session holds a [`ChatGremlinHandle`] (stored
//! on the actor-owned `SessionState`) and queues one [`PromptMsg`] per operator
//! turn onto its bounded(1) prompt channel. Between turns the gremlin is parked
//! in its session; the task takes it out at the start of each turn and re-parks
//! it when the turn ends.
//!
//! For each prompt the task:
//!
//! 1. takes the gremlin out of its session,
//! 2. installs a *fresh* per-turn [`CancelToken`] on it,
//! 3. runs exactly one agent loop turn (`next_stage` → `run_chat_stage`),
//!    publishing events to the bus, and
//! 4. restores the run-map cancel token and re-parks the gremlin.
//!
//! Two cancel tokens govern the task. A *persistent* token (the handle's
//! [`ChatGremlinHandle::cancel`]) stops the task for good — when it fires the
//! loop exits and subsequent prompt sends fail, which the session detects. A
//! per-turn *child* token is created fresh for every turn; the turn's event
//! loop races on it (the actor-owned `current_turn.cancel` is the same token)
//! so a soft/hard cancel interrupts just the in-flight turn.
//!
//! Terminal-path handling (re-park the gremlin, restore the run-map token,
//! clear `current_turn`, signal `done`, emit the terminal event) lives in
//! [`super::turn_runner`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::clients::cancel::CancelToken;
use crate::clients::client::Client as ModelClient;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::Gremlin;
use crate::executor::run_agent;
use crate::executor::stage_boundary::StageInvocation;
use gremlins_magic_bus::connection::SharedWriter;
use gremlins_magic_bus::{emit_corr_event, CorrId, EventKind};

use super::super::chat_handle::ChatGremlinHandle;
use super::super::run_registry::RunRegistry;
use super::super::session_task::SessionTaskHandle;
use super::payload::PromptMsg;
use super::turn_runner::{TurnRunner, TurnRunnerCfg};
use super::{ok_response, CurrentTurn};

/// Spawn the persistent task that drives a chat gremlin.
///
/// Registers the gremlin (parking it in its session and installing the run-map
/// entry) via the registry, then spawns a task that loops on the returned
/// handle's prompt channel. A fresh persistent [`CancelToken`] is created here
/// and stored on the handle.
///
/// Returns `None` when the registry is gone or the session no longer exists.
pub(crate) async fn spawn_chat_gremlin(
    gremlin: Gremlin,
    registry: Arc<RunRegistry>,
    session_task: SessionTaskHandle,
) -> Option<ChatGremlinHandle> {
    let id = gremlin.id.to_string();
    let cancel = CancelToken::new();
    let finished = Arc::new(AtomicBool::new(false));
    let (prompt_tx, prompt_rx) = mpsc::channel::<PromptMsg>(1);

    // Insert the run-map entry; the registry hands the gremlin back so we can
    // park it in the session. The gremlin lives in the session between turns
    // and is taken out while a turn runs.
    let gremlin = registry
        .register_chat_gremlin(id.clone(), gremlin, cancel.clone())
        .await;
    // Claim ownership *before* parking: `reinsert_gremlin` only accepts the
    // gremlin if the session already lists `id` in its chat-id set, and
    // `add_chat_id` rejects a session that is already tearing down so a
    // concurrent teardown cannot strand a live run-map entry.
    if !session_task.add_chat_id(&id).await {
        let _ = registry.stop_run(&id).await;
        return None;
    }
    if !session_task.reinsert_gremlin(&id, gremlin).await {
        // The session is gone (its loop already exited). Drop the run-map
        // entry we just installed so it does not orphan `maybe_shutdown`.
        let _ = registry.stop_run(&id).await;
        return None;
    }

    let handle = ChatGremlinHandle {
        id,
        prompt_tx,
        cancel,
        finished: finished.clone(),
    };
    // Store the handle on the session so later turns (and `cancel_chat`) can
    // reach the task's prompt channel and persistent cancel token.
    session_task.set_chat_handle(Some(handle.clone())).await;

    let task_id = handle.id.clone();
    let task_cancel = handle.cancel.clone();
    let task_registry = Arc::clone(&registry);
    let task_session = session_task.clone();
    tokio::spawn(async move {
        chat_gremlin_task(
            task_id,
            task_cancel,
            task_registry,
            task_session,
            finished,
            prompt_rx,
        )
        .await;
    });

    Some(handle)
}

/// The persistent chat-gremlin task: one turn per queued prompt, until the
/// persistent cancel token fires or every handle is dropped.
async fn chat_gremlin_task(
    id: String,
    cancel: Arc<CancelToken>,
    registry: Arc<RunRegistry>,
    session_task: SessionTaskHandle,
    finished: Arc<AtomicBool>,
    mut prompt_rx: mpsc::Receiver<PromptMsg>,
) {
    loop {
        let prompt = tokio::select! {
            biased;
            // Permanent stop: exit the loop.
            _ = cancel.cancelled() => break,
            maybe = prompt_rx.recv() => match maybe {
                Some(prompt) => prompt,
                // Every handle dropped — nothing more will ever arrive.
                None => break,
            },
        };

        #[cfg(test)]
        tokio::select! {
            _ = cancel.cancelled() => {
                publish_unstarted_end(&id, prompt.turn.as_deref());
                break;
            },
            _ = session_task.wait_turn_gate(prompt.turn.as_deref(), "start") => {}
        }
        run_turn(&id, &cancel, &registry, &session_task, prompt).await;

        // A permanent cancel that landed while the turn ran ends the task now,
        // as does a control-side `stop` that removed the run-map entry.
        if cancel.is_cancelled() || !registry.contains_run(&id).await {
            break;
        }
    }
    prompt_rx.close();
    while let Some(prompt) = prompt_rx.recv().await {
        publish_unstarted_end(&id, prompt.turn.as_deref());
    }
    finished.store(true, Ordering::Relaxed);
    log::debug!("chat gremlin task {id} exiting");
}

fn turn_corr(id: &str, turn: Option<&str>) -> CorrId {
    let corr = CorrId::new().with_run(id);
    match turn.filter(|turn| !turn.is_empty()) {
        Some(turn) => corr.with_turn_token(turn),
        None => corr,
    }
}

fn publish_unstarted_end(id: &str, turn: Option<&str>) {
    emit_corr_event(
        &turn_corr(id, turn),
        EventKind::Ended {
            reason: "gremlin stopped before turn started".into(),
        },
    );
}

/// Run exactly one turn for `prompt`, publishing events to the bus.
///
/// Takes the gremlin out of its session, installs a fresh per-turn cancel token
/// on it, runs the stage, then re-parks it on every exit path.
async fn run_turn(
    id: &str,
    persistent: &Arc<CancelToken>,
    registry: &Arc<RunRegistry>,
    session_task: &SessionTaskHandle,
    prompt: PromptMsg,
) {
    let PromptMsg {
        mut text,
        context,
        turn,
        chat_model,
        system_prompt,
        project_root,
        env,
        requested_skills,
        all_skills,
        ..
    } = prompt;

    // Take the gremlin out of the session. If it was removed underneath us
    // (a control-side `stop`), there is nothing to run — but a turn was already
    // acknowledged as streaming, so emit a terminal event for the TUI.
    let Some(mut gremlin) = session_task.take_gremlin(id).await else {
        log::debug!("chat turn {id}: gremlin gone, skipping turn");
        publish_unstarted_end(id, turn.as_deref());
        return;
    };

    #[cfg(test)]
    session_task.wait_turn_gate(turn.as_deref(), "setup").await;

    // ── Prepare the gremlin for this turn ──────────────────────────────

    // Build this turn's active skill set from the operator-requested skills
    // plus the model-loaded ones accumulated in the session, then assemble the
    // dynamic preamble (repo context + trusted skills block).
    session_task.set_operator_skills(requested_skills).await;
    let active_skills = session_task.get_combined_skills().await.unwrap_or_default();
    let (trusted_section, unresolved_skills) =
        crate::skills::build_system_prompt_section(all_skills.as_slice(), &active_skills);
    let dynamic_preamble = crate::executor::supervisor::chat_system_prompt::build_chat_preamble(
        &context,
        &trusted_section,
    );
    if !dynamic_preamble.is_empty() {
        text = format!("{dynamic_preamble}\n---\n{text}");
    }

    // A fresh per-turn child cancel token.
    let turn_cancel = CancelToken::new();
    let (done_tx, done_rx) = watch::channel(false);
    let done_tx = Arc::new(done_tx);
    let hard = Arc::new(AtomicBool::new(false));

    // Validate setup without creating an interactive execution target.
    let llm_client = ModelClient::from_uri(chat_model.clone());
    let backend = match &llm_client {
        Ok(client) => client.get_or_build_backend().map_err(|error| {
            log::warn!("chat turn {id}: failed to build backend: {error}");
            error
        }),
        Err(error) => Err(error.clone()),
    };
    gremlin.cancel_token = Some(turn_cancel.clone());

    // Announce the in-flight turn so cancel / stop / compact can find it.
    session_task
        .set_current_turn(CurrentTurn {
            id: id.to_string(),
            turn: turn.clone(),
            cancel: turn_cancel.clone(),
            done: done_rx.clone(),
            hard: hard.clone(),
            #[cfg(test)]
            stage_run_id: None,
        })
        .await;

    let corr_id = turn_corr(id, turn.as_deref());
    let mut runner = TurnRunner::new(TurnRunnerCfg {
        id: id.to_string(),
        turn,
        turn_cancel,
        persistent: persistent.clone(),
        registry: Arc::clone(registry),
        session_task: session_task.clone(),
        corr_id,
        done: done_tx,
        hard: hard.clone(),
    });

    // Rebuild the parsed model handle for the reused gremlin so a `/model`
    // change takes effect on the next turn rather than only after the chat
    // gremlin is recreated.
    let llm_client = match (llm_client, backend) {
        (Ok(client), Ok(_backend)) => client,
        (Err(error), _) => {
            let msg = format!("invalid chat model '{chat_model}': {error}");
            runner.abort(gremlin, &msg).await;
            return;
        }
        (Ok(_), Err(error)) => {
            let msg = format!("failed to build chat backend for '{chat_model}': {error}");
            runner.abort(gremlin, &msg).await;
            return;
        }
    };
    gremlin.client = llm_client;
    // Refresh per-turn fields: the chat gremlin is persistent and reused, so a
    // later turn's environment / project root must replace the earlier ones.
    gremlin.project_root = project_root;
    gremlin.env = env;
    gremlin.discovered_skills = Some(Arc::clone(&all_skills));
    // Checkpoint before set_pending consumes a prior interruption notice.
    // Restore it if cancellation wins before stage execution, including after
    // next_stage has advanced the conversation artifact slot.
    let definition_checkpoint = gremlin.definition.clone_box();
    // Queue this turn's prompt onto the ChatDefinition so `next_stage()` builds
    // the agent stage on demand.
    gremlin
        .definition
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<ChatDefinition>())
        .expect("chat gremlin must carry a ChatDefinition")
        .set_pending(&text, &chat_model, &system_prompt);
    // Keep the state.json `model` field in sync.
    let state_patch = serde_json::json!({ "model": chat_model.to_string() });
    if let Some(fields) = state_patch.as_object() {
        gremlin.state.patch(&[], fields);
    }
    // Point the run-map handle's cancel token at this turn so `stop` cancels
    // the in-flight stage rather than the persistent token.
    runner
        .registry
        .set_run_cancel(&runner.id, runner.turn_cancel.clone())
        .await;

    // ── Drive the event loop ───────────────────────────────────────────

    if persistent.is_cancelled() || runner.turn_cancel.is_cancelled() {
        runner
            .stop_before_stage(gremlin, definition_checkpoint)
            .await;
        return;
    }
    // Build the stage (consumes the pending prompt).
    let stage = match gremlin.definition.next_stage().await {
        Ok(stage) => stage,
        Err(e) => {
            let msg = format!("definition error: {e}");
            runner.abort(gremlin, &msg).await;
            return;
        }
    };

    if persistent.is_cancelled() || runner.turn_cancel.is_cancelled() {
        runner
            .stop_before_stage(gremlin, definition_checkpoint)
            .await;
        return;
    }
    let invocation =
        StageInvocation::enter_turn(id, stage.name(), stage.stage_type(), runner.turn.as_deref());
    runner.bind_corr(invocation.corr_id());
    #[cfg(test)]
    session_task
        .set_current_turn(CurrentTurn {
            id: id.to_owned(),
            turn: runner.turn.clone(),
            cancel: runner.turn_cancel.clone(),
            done: done_rx,
            hard,
            stage_run_id: Some(invocation.stage_run_id.clone()),
        })
        .await;
    runner
        .write_event(EventKind::TurnStarted {
            stage_run_id: invocation.stage_run_id.clone(),
        })
        .await;

    // Invalidate the per-turn artifact so a stale `conversation/NNNN.json`
    // from an abandoned branch is not loaded by `run_agent`.
    if let Some(turn_num) = run_agent::chat_turn_from_stage_name(stage.name()) {
        run_agent::clear_chat_turn_artifact(&gremlin, turn_num).await;
    }

    // Surface unresolved skill names as a non-fatal warning.
    if !unresolved_skills.is_empty() {
        let msg = format!("skill(s) not found: {}", unresolved_skills.join(", "));
        runner
            .write_event(EventKind::SystemWarning {
                message: msg,
                turn: runner.turn.clone(),
            })
            .await;
    }

    #[cfg(test)]
    tokio::select! {
        _ = persistent.cancelled() => {},
        _ = runner.turn_cancel.cancelled() => {},
        _ = session_task.wait_turn_gate(runner.turn.as_deref(), "incorporation") => {}
    }
    runner.run(stage, gremlin, invocation).await;
}

/// Cancel the in-flight chat turn, or (for a permanent stop) the chat task.
///
/// The `mode` field selects the token to cancel. The soft/hard turn modes cancel
/// the in-flight turn's per-turn child token; `"hard"` (double Esc) additionally
/// marks the turn for discard, and a late hard cancel clears a lingering
/// interrupted flag on the parked gremlin. `"permanent"` cancels both the
/// in-flight child token (aborting the current turn instead of letting it drain)
/// and the persistent token (ending the task; later prompt sends fail).
///
/// Acknowledges the request with an `ok` response.
pub(crate) async fn handle_cancel_chat(
    request: &Value,
    writer: &SharedWriter,
    session_task: &SessionTaskHandle,
) {
    let turn = request
        .get("turn")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mode = request
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("soft");

    // A permanent stop cancels both tokens: the in-flight turn's child token
    // (so the turn aborts now instead of draining to completion) and the
    // persistent token (so the task exits and later prompt sends fail).
    if mode == "permanent" {
        if let Some(handle) = session_task.take_chat_handle().await {
            let snapshot = session_task.get_current_turn().await;
            if snapshot.as_ref().is_some_and(|cur| cur.id == handle.id) {
                let _ = session_task.cancel_turn(Some(&handle.id)).await;
            }
            handle.cancel.cancel();
        }
        let _ = writer
            .write_json_line(&ok_response(serde_json::json!({})))
            .await;
        return;
    }

    let snapshot = session_task.get_current_turn().await;
    let matches = match &snapshot {
        Some(cur) => turn
            .as_deref()
            .is_none_or(|t| Some(t) == cur.turn.as_deref()),
        None => false,
    };
    if !matches {
        if mode == "hard" {
            // Clear any lingering interrupted flag from a prior soft cancel on
            // parked chat gremlins so the next "start typing" does not see a
            // stale interruption.
            let keys = session_task.get_chat_gremlin_keys().await;
            for id in keys {
                if let Some(mut gremlin) = session_task.take_gremlin(&id).await {
                    if let Some(chat) = gremlin
                        .definition
                        .as_any_mut()
                        .and_then(|a| a.downcast_mut::<ChatDefinition>())
                    {
                        chat.clear_interrupted();
                    }
                    let _ = session_task.reinsert_gremlin(&id, gremlin).await;
                }
            }
        }
        let _ = writer
            .write_json_line(&ok_response(serde_json::json!({})))
            .await;
        return;
    }
    let cur = snapshot.expect("matched turn is present");

    // Hard cancel: communicate the intent to the turn loop through the shared
    // atomic flag before signalling the cancel token.
    if mode == "hard" {
        session_task.set_turn_hard().await;
    }
    let _ = session_task.cancel_turn(Some(&cur.id)).await;

    let _ = writer
        .write_json_line(&ok_response(serde_json::json!({})))
        .await;
}
