//! Chat turn execution: the turn loop, cancellation, and the shared
//! [`TurnCtx`] context.
//!
//! Request handling and gremlin lifecycle live in [`super`]; terminal-path
//! resources live in [`turn_finalizer`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::clients::cancel::CancelToken;
use crate::definition::{ChatDefinition, ExecutorStage};
use crate::executor::gremlin::Gremlin;
use crate::executor::run::run_stage;
use crate::executor::run_agent;
use crate::executor::RunError;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run_backpressured, EventKind, Recv};

use super::super::actor::ActorMsg;
use super::super::actor_client::Client;
use super::super::ok_response;
use super::payload::{add_usage_stats, chat_payload, usage_stats_from_state};
use super::turn_finalizer::{TurnEnd, TurnFinalizer};

/// Shared context threaded through a chat turn's forwarding task.
pub(crate) struct TurnCtx {
    pub(crate) id: String,
    pub(crate) turn: Option<String>,
    pub(crate) turn_cancel: Arc<CancelToken>,
    /// Mailbox for the supervisor actor — the turn reaches session and
    /// run-map state exclusively through it.
    pub(crate) actor_tx: mpsc::UnboundedSender<ActorMsg>,
    pub(crate) session_id: String,
    pub(crate) done: Arc<watch::Sender<bool>>,
    pub(crate) hard: Arc<AtomicBool>,
}

pub(crate) async fn run_chat_turn(
    ctx: TurnCtx,
    stage: ExecutorStage,
    mut gremlin: Gremlin,
    writer: SharedWriter,
) {
    let id = ctx.id;
    let turn = ctx.turn;
    let turn_cancel = ctx.turn_cancel;

    // Subscribe to this run's stream events *before* spawning the stage
    // runner so no early event (Ready, first chunks) is missed. The bounded
    // buffer makes a slow front-end apply backpressure to the agent loop
    // rather than dropping live stream events. The guard moves into the
    // TurnFinalizer so it's dropped before the gremlin is re-inserted —
    // otherwise the next turn (same run id) fills the old, undrained mailbox.
    let (stream_sub, mut stream_rx) = subscribe_run_backpressured(&id, 256);
    let stream_guard = BusSubGuard::new(stream_sub);

    // Terminal-path resources live on the finalizer so every exit path
    // re-inserts the gremlin and clears the in-flight turn the same way.
    let finalizer = TurnFinalizer::new(
        &ctx.actor_tx,
        &ctx.session_id,
        &id,
        &turn,
        &turn_cancel,
        &ctx.done,
        writer.clone(),
        stream_guard,
    );

    // Capture the turn number from the stage name so cancel cleanup
    // can always clear the per-turn artifact.
    let chat_turn_num = run_agent::chat_turn_from_stage_name(stage.name());

    // Capture the conversation baseline from the gremlin's persisted state
    // *before* moving it into the stage runner. `run_stage` accumulates the
    // current prompt's usage into the same state after completion, so this
    // must be a pre-turn snapshot. Missing state (the first chat prompt) is
    // treated as zero.
    let baseline_usage = usage_stats_from_state(gremlin.state.read_field("token_usage").as_ref());

    // Spawn the stage runner so we can select on it alongside stream events.
    let mut run_task = tokio::spawn(async move {
        let result = run_stage(&stage, &mut gremlin).await;
        (result, gremlin)
    });

    // Pre-construct the actor client so SkillLoaded events don't clone
    // the sender on every occurrence.
    let actor_client = Client::new(ctx.actor_tx.clone());

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
                let event = match result {
                    Recv::Event(dispatch) => dispatch.into_kind(),
                    // Backpressured receivers never lag; ignore defensively.
                    Recv::Lagged(_) => continue,
                    Recv::Closed => {
                        let ev = EventKind::Error {
                            message: "agent exited before completing".to_string(),
                            turn,
                        };
                        finalizer
                            .stream_terminal(
                                &mut run_task,
                                serde_json::to_value(&ev).unwrap_or_default(),
                            )
                            .await;
                        return;
                    }
                };
                match event {
                    EventKind::StreamChunk { .. }
                    | EventKind::ReasoningChunk { .. }
                    | EventKind::ToolOutputChunk { .. }
                    | EventKind::StreamReset
                    | EventKind::ToolResult { .. }
                    | EventKind::TaskProgress { .. } => {
                        send!(serde_json::to_value(&event).unwrap_or_default());
                    }
                    EventKind::TurnComplete { turn, text, tool_calls, stats, totals, .. } => {
                        let total = totals.as_ref().map(|t| add_usage_stats(&baseline_usage, t));
                        let ev = EventKind::TurnComplete {
                            turn,
                            text,
                            tool_calls,
                            stats,
                            totals: total,
                        };
                        send!(serde_json::to_value(&ev).unwrap_or_default());
                    }
                    EventKind::Done { text, summary, usage } => {
                        let usage = usage.as_ref().map(|u| add_usage_stats(&baseline_usage, u));
                        let ev = EventKind::Done { text, summary, usage };
                        finalizer
                            .stream_terminal(
                                &mut run_task,
                                serde_json::to_value(&ev).unwrap_or_default(),
                            )
                            .await;
                        return;
                    }
                    EventKind::Ended { .. } => {
                        finalizer
                            .stream_terminal(
                                &mut run_task,
                                serde_json::to_value(&event).unwrap_or_default(),
                            )
                            .await;
                        return;
                    }
                    EventKind::Ready { .. } => {
                        // Ignore Ready in chat mode.
                    }
                    EventKind::Error { .. } => {
                        // Forward errors from the bus (e.g. agent exit).
                        send!(serde_json::to_value(&event).unwrap_or_default());
                    }
                    EventKind::SystemWarning { .. } => {
                        send!(serde_json::to_value(&event).unwrap_or_default());
                    }
                    EventKind::SkillLoaded { name, .. } => {
                        // Append the model-loaded skill to the session so the
                        // next turn's system prompt picks it up.
                        actor_client
                            .session_append_model_skills(
                                &ctx.session_id,
                                vec![name],
                            )
                            .await;
                    }
                    // Other run-scoped events (log lines, token usage, run
                    // lifecycle) are not part of the chat stream protocol.
                    _ => {}
                }
            }
            _ = turn_cancel.cancelled() => {
                // Cancel: the caller (handle_cancel_chat) already signalled
                // the cancel token. The soft path marks ChatDefinition::interrupted
                // so the next turn injects an interrupt notice. Partial messages
                // are in-memory only and intentionally lost on cancel; the
                // per-turn artifact (typically absent on this path) is preserved
                // rather than cleared.
                match run_task.await {
                    Ok((Err(RunError::Cancelled), mut recovered)) => {
                        // Check the hard flag: hard cancel (double-Esc) discards
                        // the turn; soft cancel (single Esc) marks interrupted.
                        if ctx.hard.load(Ordering::Relaxed) {
                            if let Some(chat) = recovered.definition.as_any_mut()
                                .and_then(|a| a.downcast_mut::<ChatDefinition>())
                            {
                                chat.clear_interrupted();
                            }
                            finalizer.cancel(Some(recovered), chat_turn_num, TurnEnd::Hard).await;
                        } else {
                            finalizer.cancel(Some(recovered), chat_turn_num, TurnEnd::Soft).await;
                        }
                        return;
                    }
                    Ok((_, gremlin)) => {
                        // Hard path: agent completed or errored non-cancel.
                        finalizer.cancel(Some(gremlin), chat_turn_num, TurnEnd::Hard).await;
                        return;
                    }
                    Err(join_err) => {
                        // Run task panicked or was cancelled. Clean up without
                        // a gremlin but still clear current_turn + signal done.
                        let message = if join_err.is_panic() {
                            "agent panicked".to_string()
                        } else {
                            format!("agent task cancelled: {join_err}")
                        };
                        finalizer
                            .cancel(None, chat_turn_num, TurnEnd::Fallback { reason: message })
                            .await;
                        return;
                    }
                }
            }

            task_result = &mut run_task => {
                // The run task finished without a terminal event on the
                // stream channel. This is the error path: `run_stage`
                // returned Err, the agent panicked, or the task was
                // cancelled. The bus subscriber is still held by this task's
                // `BusSubGuard`, so `stream_rx` would block forever without
                // this branch.
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
                // Re-insert the gremlin (if we have one), clear the per-turn
                // artifact, clear the in-flight turn, and emit `ended` with the
                // failure reason. Do NOT call finish() here — run_task was
                // already polled to completion by the select, and awaiting it
                // again is undefined.
                finalizer
                    .cancel(
                        gremlin_for_reinsert,
                        chat_turn_num,
                        TurnEnd::Fallback { reason: message },
                    )
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

/// Cancel the in-flight chat turn. The `mode` field determines soft vs
/// hard: "hard" (double-Esc) clears any interrupted flag and discards the
/// turn; "soft" (single Esc) marks the ChatDefinition interrupted so the
/// next prompt prepends an interrupt notice.
///
/// After soft finalization has already cleared `current_turn`, a late hard
/// cancel still clears the interrupted flag on parked chat gremlins so the
/// next "start typing" does not see a stale interruption.
pub(crate) async fn handle_cancel_chat(
    request: &Value,
    writer: &SharedWriter,
    actor_tx: &mpsc::UnboundedSender<ActorMsg>,
    session_id: &str,
) {
    let turn = request
        .get("turn")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mode = request
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("soft");

    let client = Client::new(actor_tx.clone());
    let snapshot = client.session_get_current_turn(session_id).await;
    let matches = match &snapshot {
        Some(cur) => turn
            .as_deref()
            .is_none_or(|t| Some(t) == cur.turn.as_deref()),
        None => false,
    };
    if !matches {
        if mode == "hard" {
            // Clear any lingering interrupted flag from a prior soft cancel.
            let keys = client.session_get_chat_gremlin_keys(session_id).await;
            for id in keys {
                if let Some(mut gremlin) = client.session_take_gremlin(session_id, &id).await {
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
            }
        }
        let _ = writer
            .write_json_line(&ok_response(serde_json::json!({})))
            .await;
        return;
    }
    let cur = snapshot.expect("matched turn is present");

    // Hard cancel: communicate the intent to `run_chat_turn` through the
    // shared atomic flag before signalling the cancel token.
    if mode == "hard" {
        client.session_set_turn_hard(session_id).await;
    }

    let _ = client.session_cancel_turn(session_id, Some(&cur.id)).await;

    let _ = writer
        .write_json_line(&ok_response(serde_json::json!({})))
        .await;
}
