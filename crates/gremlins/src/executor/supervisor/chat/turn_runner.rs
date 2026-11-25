//! Bus observation and terminal cleanup for one chat turn.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinError;

use crate::clients::cancel::CancelToken;
use crate::definition::{ExecutorStage, GremlinDefinition};
use crate::executor::gremlin::Gremlin;
use crate::executor::run::run_chat_stage;
use crate::executor::run_agent;
use crate::executor::stage_boundary::StageInvocation;
use crate::executor::RunError;
use gremlins_magic_bus::connection::BusSubGuard;
use gremlins_magic_bus::{emit_corr_event, subscribe_run, CorrId, EventKind, Recv};

use super::super::run_registry::RunRegistry;
use super::super::session_task::SessionTaskHandle;

enum TurnEnd {
    Complete,
    Cancelled {
        reason: &'static str,
        discard_artifact: bool,
    },
    Failed(String),
}

#[cfg(test)]
#[path = "turn_runner_tests.rs"]
mod tests;

type RunTaskOutput = (Result<(), RunError>, Gremlin);

/// Owns re-parking, cancel-token restoration, and completion notification.
pub(super) struct TurnRunner {
    pub(super) id: String,
    pub(super) turn: Option<String>,
    pub(super) turn_cancel: Arc<CancelToken>,
    pub(super) registry: Arc<RunRegistry>,
    session_task: SessionTaskHandle,
    persistent: Arc<CancelToken>,
    corr_id: CorrId,
    done: Arc<watch::Sender<bool>>,
    hard: Arc<AtomicBool>,
}

pub(super) struct TurnRunnerCfg {
    pub id: String,
    pub turn: Option<String>,
    pub turn_cancel: Arc<CancelToken>,
    pub persistent: Arc<CancelToken>,
    pub registry: Arc<RunRegistry>,
    pub session_task: SessionTaskHandle,
    pub corr_id: CorrId,
    pub done: Arc<watch::Sender<bool>>,
    pub hard: Arc<AtomicBool>,
}

impl TurnRunner {
    pub(super) fn new(cfg: TurnRunnerCfg) -> Self {
        Self {
            id: cfg.id,
            turn: cfg.turn,
            turn_cancel: cfg.turn_cancel,
            persistent: cfg.persistent,
            registry: cfg.registry,
            session_task: cfg.session_task,
            corr_id: cfg.corr_id,
            done: cfg.done,
            hard: cfg.hard,
        }
    }

    pub(super) fn bind_corr(&mut self, corr_id: CorrId) {
        self.corr_id = corr_id;
    }

    pub(super) async fn stop_before_stage(
        &self,
        mut gremlin: Gremlin,
        checkpoint: Box<dyn GremlinDefinition>,
    ) {
        // Restore both the consumed slot and any prior interruption notice.
        gremlin.definition = checkpoint;
        if self.hard.load(Ordering::Relaxed) {
            if let Some(chat) = gremlin
                .definition
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<crate::definition::ChatDefinition>())
            {
                chat.clear_interrupted();
            }
        }
        self.reinsert(gremlin).await;
        self.write_event(EventKind::Ended {
            reason: "cancelled before stage started".into(),
        })
        .await;
        self.clear_turn_and_signal().await;
    }

    pub(super) async fn run(
        &self,
        stage: ExecutorStage,
        mut gremlin: Gremlin,
        invocation: StageInvocation,
    ) {
        // Lossless/unbounded bookkeeping, independent of lossy display readers.
        // Subscribe before spawning so neither skills nor Done can be lost.
        let (sub, mut rx) = subscribe_run(&self.id);
        let guard = BusSubGuard::new(sub);
        let turn_num = run_agent::chat_turn_from_stage_name(stage.name());
        let debug_terminal = gremlin
            .interactive_sessions
            .as_mut()
            .map(|sessions| sessions.supervise_next_turn());
        let mut task = tokio::spawn(async move {
            let result = run_chat_stage(&stage, &mut gremlin, invocation).await;
            (result, gremlin)
        });
        let mut saw_done = false;
        let result = loop {
            tokio::select! {
                result = &mut task => break result,
                event = rx.recv() => match event {
                    Recv::Event(event) => self.record_producer_event(event.into_kind(), &mut saw_done).await,
                    Recv::Lagged(_) => continue,
                    Recv::Closed => break task.await,
                },
            }
        };
        // Detach first: late run publishers must not extend terminal cleanup.
        // Lossless sends are synchronous, so the joined producer's events are
        // already queued. Snapshot a finite drain even if a sender is in flight.
        drop(guard);
        let pending = match &rx {
            gremlins_magic_bus::ReceiverHalf::Lossless(rx) => rx.len(),
            _ => unreachable!("turn bookkeeping requires lossless delivery"),
        };
        for _ in 0..pending {
            let Ok(event) = rx.try_recv() else { break };
            self.record_producer_event(event.into_kind(), &mut saw_done)
                .await;
        }

        let (gremlin, end) = self.classify_task_result(result, saw_done);
        if let (Some(terminal), TurnEnd::Cancelled { reason, .. }) = (&debug_terminal, &end) {
            terminal.end(reason);
        }
        let discard = match &end {
            TurnEnd::Complete => false,
            TurnEnd::Cancelled {
                discard_artifact, ..
            } => *discard_artifact,
            TurnEnd::Failed(_) => true,
        };
        if let Some(gremlin) = gremlin {
            if discard {
                if let Some(turn_num) = turn_num {
                    run_agent::clear_chat_turn_artifact(&gremlin, turn_num).await;
                }
            }
            self.reinsert(gremlin).await;
        } else {
            // The stage task lost its gremlin. Retire the prompt task rather
            // than accepting further turns that can never execute.
            self.persistent.cancel();
            let _ = self.registry.stop_run(&self.id).await;
        }
        match end {
            TurnEnd::Complete => {}
            TurnEnd::Cancelled { reason, .. } => {
                self.write_event(EventKind::Ended {
                    reason: reason.into(),
                })
                .await;
            }
            TurnEnd::Failed(message) => self.publish_failure(&self.corr_id, message),
        }
        self.clear_turn_and_signal().await;
    }

    async fn record_producer_event(&self, event: EventKind, saw_done: &mut bool) {
        match event {
            EventKind::SkillLoaded { name, .. } => {
                self.session_task.append_model_skills(vec![name]).await;
            }
            EventKind::Done { .. } => *saw_done = true,
            _ => {}
        }
    }

    fn classify_task_result(
        &self,
        result: Result<RunTaskOutput, JoinError>,
        saw_done: bool,
    ) -> (Option<Gremlin>, TurnEnd) {
        match result {
            Ok((Err(RunError::Cancelled), mut gremlin)) if self.turn_cancel.is_cancelled() => {
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
                (
                    Some(gremlin),
                    TurnEnd::Cancelled {
                        reason: if hard { "cancelled" } else { "interrupted" },
                        discard_artifact: hard,
                    },
                )
            }
            Ok((Err(error), gremlin)) => (
                Some(gremlin),
                TurnEnd::Failed(format!("agent error: {error}")),
            ),
            // Once the producer completed, a late stop cannot create a second terminal.
            Ok((Ok(()), gremlin)) if saw_done => (Some(gremlin), TurnEnd::Complete),
            Ok((Ok(()), gremlin)) if self.turn_cancel.is_cancelled() => (
                Some(gremlin),
                TurnEnd::Cancelled {
                    reason: "cancelled",
                    discard_artifact: true,
                },
            ),
            Ok((Ok(()), gremlin)) => (
                Some(gremlin),
                TurnEnd::Failed("agent completed without terminal event".into()),
            ),
            Err(error) if error.is_panic() => (None, TurnEnd::Failed("agent panicked".into())),
            Err(error) => (
                None,
                TurnEnd::Failed(format!("agent task cancelled: {error}")),
            ),
        }
    }

    pub(super) async fn abort(&self, mut gremlin: Gremlin, message: &str) {
        gremlin.discard_prepared_interactive_session();
        self.reinsert(gremlin).await;
        let mut corr = CorrId::new().with_run(&self.id);
        if let Some(turn) = self.turn.as_deref().filter(|turn| !turn.is_empty()) {
            corr = corr.with_turn_token(turn);
        }
        self.publish_failure(&corr, message.into());
        self.clear_turn_and_signal().await;
    }

    pub(super) async fn write_event(&self, event: EventKind) {
        emit_corr_event(&self.corr_id, event);
    }

    /// Error is diagnostic, not completion. Ended is the failed-turn terminal;
    /// consumers must wait for Ended rather than completing twice on this pair.
    fn publish_failure(&self, corr: &CorrId, message: String) {
        emit_corr_event(
            corr,
            EventKind::Error {
                message: message.clone(),
                turn: self.turn.clone(),
            },
        );
        emit_corr_event(corr, EventKind::Ended { reason: message });
    }

    async fn clear_turn_and_signal(&self) {
        self.session_task
            .clear_current_turn(self.turn_cancel.clone())
            .await;
        let _ = self.done.send(true);
    }

    async fn restore_run_cancel(&self) {
        self.registry
            .set_run_cancel(&self.id, self.persistent.clone())
            .await;
    }

    async fn reinsert(&self, mut gremlin: Gremlin) {
        gremlin.cancel_token = None;
        self.restore_run_cancel().await;
        self.session_task.reinsert_gremlin(&self.id, gremlin).await;
    }
}
