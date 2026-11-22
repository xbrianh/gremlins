//! Turn runner: owns the event loop and terminal-path handling for one
//! chat turn.
//!
//! Extracted from [`super::turn`] so the turn orchestration stays readable —
//! the persistent task and per-turn preparation live in `turn`, while the
//! event loop, bus subscription, and "re-park the gremlin" unwinding live here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};

use crate::clients::cancel::CancelToken;
use crate::clients::interactive::MESSAGE_DELIVERED_REASON;
use crate::definition::ExecutorStage;
use crate::executor::gremlin::Gremlin;
use crate::executor::run::run_stage;
use crate::executor::run_agent;
use crate::executor::RunError;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run_backpressured, EventKind, Recv};

use super::super::run_registry::RunRegistry;
use super::super::session_task::SessionTaskHandle;
use super::payload::{add_usage_stats, chat_payload, usage_stats_from_state};

// ── Internal outcome type ───────────────────────────────────────────────

/// How a chat turn ended, used to classify the terminal unwinding path.
enum TurnEnd {
    /// A stream terminal event arrived (Done, Ended, Error from stream).
    /// The payload is written after the gremlin is re-parked.
    Terminal(Value),
    /// The turn was cancelled. `reason` is the `Ended.reason` string.
    /// `discard_artifact` controls whether the per-turn conversation
    /// artifact is cleared.
    Cancelled {
        reason: String,
        discard_artifact: bool,
    },
}

/// A stage returns the gremlin to the event loop even on error.
type RunTaskOutput = (Result<(), RunError>, Gremlin);
type RunTaskHandle = JoinHandle<RunTaskOutput>;

// ── TurnRunner ──────────────────────────────────────────────────────────

/// Terminal-path resources for one chat turn.
///
/// Holds everything a turn needs to unwind: the run registry, the per-turn and
/// persistent cancel tokens, the `done` broadcaster, the connection writer,
/// and the bus subscription guard. Every exit path goes through one of its
/// methods so the "re-park the gremlin, restore the run-map token, clear
/// `current_turn`, signal `done`, emit the terminal event" dance happens
/// exactly once.
///
/// Constructed by [`new`](TurnRunner::new); most fields are private so
/// external code cannot bypass the terminal-path invariants.  The fields that
/// `turn.rs` needs during turn setup (`id`, `turn`, `turn_cancel`, `registry`)
/// are `pub(super)`.
pub(super) struct TurnRunner {
    /// The gremlin id this turn drives.
    pub(super) id: String,
    /// The client-generated turn id, echoed on chat events.
    pub(super) turn: Option<String>,
    /// The per-turn cancel token, installed on the gremlin and run map
    /// before [`run`](TurnRunner::run) is called.
    pub(super) turn_cancel: Arc<CancelToken>,
    /// The run registry for the turn. Public so `turn.rs` can call
    /// `set_run_cancel` during turn setup.
    pub(super) registry: Arc<RunRegistry>,
    /// Handle to the owning session task; all session-scoped state (current
    /// turn, parked gremlins, skills) is reached through it.
    session_task: SessionTaskHandle,
    /// The task's persistent token, restored on the run map after each turn.
    persistent: Arc<CancelToken>,
    writer: SharedWriter,
    done: Arc<watch::Sender<bool>>,
    hard: Arc<AtomicBool>,
}

/// Config bag for [`TurnRunner::new`] — keeps the arg count under clippy's
/// limit without sacrificing clarity.
pub(super) struct TurnRunnerCfg {
    pub id: String,
    pub turn: Option<String>,
    pub turn_cancel: Arc<CancelToken>,
    pub persistent: Arc<CancelToken>,
    pub registry: Arc<RunRegistry>,
    pub session_task: SessionTaskHandle,
    pub writer: SharedWriter,
    pub done: Arc<watch::Sender<bool>>,
    pub hard: Arc<AtomicBool>,
}

impl TurnRunner {
    /// Build a new runner from the pieces assembled in [`super::turn::run_turn`].
    pub(super) fn new(cfg: TurnRunnerCfg) -> Self {
        let TurnRunnerCfg {
            id,
            turn,
            turn_cancel,
            persistent,
            registry,
            session_task,
            writer,
            done,
            hard,
        } = cfg;
        Self {
            id,
            turn,
            turn_cancel,
            persistent,
            registry,
            session_task,
            writer,
            done,
            hard,
        }
    }

    /// Run the stage and forward its stream events until a terminal event,
    /// a cancellation, or a write error.
    pub(super) async fn run(&self, stage: ExecutorStage, mut gremlin: Gremlin) {
        // Subscribe *before* spawning the stage runner so no early event (the
        // first chunks) is missed. The bounded buffer applies backpressure to
        // the agent loop instead of dropping live stream events.
        let (stream_sub, mut stream_rx) = subscribe_run_backpressured(&self.id, 256);
        let mut stream_guard = Some(BusSubGuard::new(stream_sub));

        // Capture the turn number from the stage name so cancel cleanup can
        // always clear the per-turn artifact.
        let chat_turn_num = run_agent::chat_turn_from_stage_name(stage.name());

        // Capture the conversation baseline *before* moving the gremlin into
        // the stage runner (`run_stage` accumulates this prompt's usage into
        // the same state after completion).
        let baseline_usage =
            usage_stats_from_state(gremlin.state.read_field("token_usage").as_ref());

        let debug_terminal = gremlin
            .interactive_sessions
            .as_mut()
            .map(|sessions| sessions.supervise_next_turn());
        let mut run_task: RunTaskHandle = tokio::spawn(async move {
            let result = run_stage(&stage, &mut gremlin).await;
            (result, gremlin)
        });

        let (result, writer_live) = loop {
            macro_rules! send {
                ($fields:expr) => {
                    let payload = chat_payload(&self.turn, $fields);
                    if self.writer.write_json_line(&payload).await.is_err() {
                        // No consumer remains for the bounded chat stream.
                        Self::drop_guard(&mut stream_guard);
                        drop(stream_rx);
                        break (run_task.await, false);
                    }
                };
            }
            let result = tokio::select! {
                biased;

                result = stream_rx.recv() => {
                    let event = match result {
                        Recv::Event(dispatch) => dispatch.into_kind(),
                        // Backpressured receivers never lag; ignore defensively.
                        Recv::Lagged(_) => continue,
                        Recv::Closed => {
                            let ev = EventKind::Error {
                                message: "agent exited before completing".to_string(),
                                turn: self.turn.clone(),
                            };
                            let payload = self.dispatch_terminal(
                                TurnEnd::Terminal(
                                    serde_json::to_value(&ev).unwrap_or_default(),
                                ),
                                None,
                                &mut stream_guard,
                                Some(&mut run_task),
                                chat_turn_num,
                            )
                            .await;
                            self.write_payload(payload).await;
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
                        EventKind::TurnComplete { turn, text, tool_calls, stats, totals } => {
                            let total = totals.as_ref().map(|t| add_usage_stats(&baseline_usage, t));
                            let ev = EventKind::TurnComplete { turn, text, tool_calls, stats, totals: total };
                            send!(serde_json::to_value(&ev).unwrap_or_default());
                        }
                        EventKind::Done { text, summary, usage } => {
                            let usage = usage.as_ref().map(|u| add_usage_stats(&baseline_usage, u));
                            let ev = EventKind::Done { text, summary, usage };
                            let payload = self.dispatch_terminal(
                                TurnEnd::Terminal(
                                    serde_json::to_value(&ev).unwrap_or_default(),
                                ),
                                None,
                                &mut stream_guard,
                                Some(&mut run_task),
                                chat_turn_num,
                            )
                            .await;
                            self.write_payload(payload).await;
                            return;
                        }
                        // Detaching the debugger resumes the model. Keep draining
                        // the chat stream until the turn itself has ended.
                        EventKind::Ended { ref reason }
                            if matches!(reason.as_str(), "resumed" | "disconnect" | MESSAGE_DELIVERED_REASON) => {}
                        EventKind::Ended { .. } => {
                            let payload = self.dispatch_terminal(
                                TurnEnd::Terminal(
                                    serde_json::to_value(&event).unwrap_or_default(),
                                ),
                                None,
                                &mut stream_guard,
                                Some(&mut run_task),
                                chat_turn_num,
                            )
                            .await;
                            self.write_payload(payload).await;
                            return;
                        }
                        // Ignore Ready in chat mode.
                        EventKind::Ready { .. } => {}
                        EventKind::Error { .. } | EventKind::SystemWarning { .. } => {
                            send!(serde_json::to_value(&event).unwrap_or_default());
                        }
                        EventKind::SkillLoaded { name, .. } => {
                            // Append the model-loaded skill to the session so
                            // the next turn's system prompt picks it up.
                            self.session_task
                                .append_model_skills(vec![name])
                                .await;
                        }
                        // Other run-scoped events are not part of the chat
                        // stream protocol.
                        _ => {}
                    }
                    continue;
                }

                _ = self.turn_cancel.cancelled() => {
                    // Stop backpressuring the stage while we wait for it to unwind.
                    Self::drop_guard(&mut stream_guard);
                    drop(stream_rx);
                    run_task.await
                }
                result = &mut run_task => result,
            };
            break (result, true);
        };
        let (gremlin, end) = self.classify_task_result(result);
        if let (Some(terminal), TurnEnd::Cancelled { reason, .. }) = (&debug_terminal, &end) {
            terminal.end(reason);
        }
        let payload = self
            .dispatch_terminal(end, gremlin, &mut stream_guard, None, chat_turn_num)
            .await;
        if writer_live {
            self.write_payload(payload).await;
        }
    }

    /// A cancelled stage can finish between polling the cancel token and the
    /// join handle. Its history policy must not depend on which arm won.
    fn classify_task_result(
        &self,
        result: Result<RunTaskOutput, JoinError>,
    ) -> (Option<Gremlin>, TurnEnd) {
        let cancelled = self.turn_cancel.is_cancelled();
        let (gremlin, reason, discard_artifact) = match result {
            Ok((Err(RunError::Cancelled), mut gremlin)) if cancelled => {
                let hard = self.hard.load(Ordering::Relaxed);
                if let Some(chat) = gremlin
                    .definition
                    .as_any_mut()
                    .and_then(|a| a.downcast_mut::<crate::definition::ChatDefinition>())
                {
                    if hard {
                        chat.clear_interrupted();
                    } else {
                        chat.mark_interrupted();
                    }
                }
                let reason = if hard { "cancelled" } else { "interrupted" };
                (Some(gremlin), reason.into(), hard)
            }
            Ok((_, gremlin)) if cancelled => (Some(gremlin), "cancelled".into(), true),
            Ok((Ok(()), gremlin)) => (
                Some(gremlin),
                "agent completed without terminal event".into(),
                true,
            ),
            Ok((Err(e), gremlin)) => (Some(gremlin), format!("agent error: {e}"), true),
            Err(e) if e.is_panic() => (None, "agent panicked".into(), true),
            Err(e) => (None, format!("agent task cancelled: {e}"), true),
        };
        (
            gremlin,
            TurnEnd::Cancelled {
                reason,
                discard_artifact,
            },
        )
    }

    /// Abort turn setup before the stage runner consumes its prepared session
    /// (invalid model, definition error): re-park the gremlin, clear the
    /// in-flight turn, and report the error.
    pub(super) async fn abort(&self, mut gremlin: Gremlin, message: &str) {
        // The chat turn eagerly published an interactive session before
        // validating the model/building its stage. If this aborts before the
        // stage runner consumes that session, drop it before parking the
        // gremlin so the execution is revoked rather than left live.
        gremlin.discard_prepared_interactive_session();
        gremlin.cancel_token = None;
        self.restore_run_cancel().await;
        self.session_task.reinsert_gremlin(&self.id, gremlin).await;
        self.clear_turn_and_signal().await;
        self.write_payload(
            serde_json::to_value(EventKind::Error {
                message: message.to_string(),
                turn: self.turn.clone(),
            })
            .unwrap_or_default(),
        )
        .await;
    }

    /// Write a chat payload to the connection, tagged with this turn's id.
    /// Public so `turn.rs` can emit events during the preparation phase
    /// (e.g. unresolved-skill warnings) before entering `run`.
    pub(super) async fn write(&self, payload: Value) {
        self.write_payload(payload).await;
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Single terminal dispatch: matches [`TurnEnd`] and executes the right
    /// unwinding path. There is no silent fall-through — both variants are
    /// handled exhaustively via a `match`, so the compiler guarantees every
    /// outcome is handled.
    ///
    /// Callers that already awaited the run task (cancel and task-completion
    /// arms) pass the recovered gremlin as `gremlin_opt` and `None` for
    /// `run_task`; stream-terminal callers pass `Some(run_task)` and `None`
    /// for `gremlin_opt`.
    async fn dispatch_terminal(
        &self,
        end: TurnEnd,
        gremlin_opt: Option<Gremlin>,
        guard: &mut Option<BusSubGuard>,
        run_task: Option<&mut RunTaskHandle>,
        chat_turn_num: Option<usize>,
    ) -> Value {
        match end {
            TurnEnd::Terminal(payload) => {
                // Stream-terminal: gremlin not yet recovered; finish() does it.
                let rt = run_task.expect("Terminal path requires run_task");
                self.finish(guard, rt).await;
                payload
            }
            TurnEnd::Cancelled {
                reason,
                discard_artifact,
            } => {
                // The gremlin was already recovered by the caller (cancel or
                // task-completion arm).  If one is present, optionally clear
                // the per-turn artifact before re-parking it.
                if let Some(gremlin) = gremlin_opt {
                    if discard_artifact {
                        if let Some(turn_num) = chat_turn_num {
                            run_agent::clear_chat_turn_artifact(&gremlin, turn_num).await;
                        }
                    }
                    self.reinsert(guard, gremlin).await;
                } else {
                    Self::drop_guard(guard);
                    self.restore_run_cancel().await;
                }
                self.clear_turn_and_signal().await;
                serde_json::to_value(EventKind::Ended { reason }).unwrap_or_default()
            }
        }
    }

    /// Write a chat payload to the connection, tagged with this turn's id.
    async fn write_payload(&self, payload: Value) {
        let payload = chat_payload(&self.turn, payload);
        let _ = self.writer.write_json_line(&payload).await;
    }

    /// Clear `current_turn` on the session (only when it still points at this
    /// turn's cancel token), then publish `true` on `done`.
    async fn clear_turn_and_signal(&self) {
        self.session_task
            .clear_current_turn(self.turn_cancel.clone())
            .await;
        let _ = self.done.send(true);
    }

    /// Drop the bus subscription guard so the next turn (which reuses the
    /// same run id) gets its own fresh subscription instead of filling this
    /// turn's undrained mailbox.
    fn drop_guard(guard: &mut Option<BusSubGuard>) {
        drop(guard.take());
    }

    /// Restore the run map's cancel token to the task's persistent one.
    async fn restore_run_cancel(&self) {
        self.registry
            .set_run_cancel(&self.id, self.persistent.clone())
            .await;
    }

    /// Re-park the gremlin in its session, restoring the run-map cancel token
    /// to the persistent one first.
    async fn reinsert(&self, guard: &mut Option<BusSubGuard>, mut gremlin: Gremlin) {
        Self::drop_guard(guard);
        gremlin.cancel_token = None;
        self.restore_run_cancel().await;
        self.session_task.reinsert_gremlin(&self.id, gremlin).await;
    }

    /// Await the run task, re-park the gremlin if the task returned one, then
    /// clear the in-flight turn and signal completion.
    async fn finish(&self, guard: &mut Option<BusSubGuard>, run_task: &mut RunTaskHandle) {
        match run_task.await {
            Ok((_, gremlin)) => self.reinsert(guard, gremlin).await,
            Err(e) => {
                log::warn!("chat turn {}: run task panicked: {e}", self.id);
                Self::drop_guard(guard);
                self.restore_run_cancel().await;
            }
        }
        self.clear_turn_and_signal().await;
    }
}
