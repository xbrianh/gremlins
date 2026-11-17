//! Terminal-path resources for a chat turn.
//!
//! [`TurnFinalizer`] owns everything a chat turn needs on its terminal paths
//! (re-insertion, `current_turn` cleanup, `done` signalling, and the
//! connection writer) and consolidates the repetitive re-insert-and-clear
//! sequences that `run_chat_turn` used to spell out inline. Each method is a
//! faithful extraction of one of those sequences.
//!
//! Session and run-map state is actor-owned; the finalizer reaches it through
//! [`ActorMsg`] messages carried by the [`TurnCtx`](super::TurnCtx) mailbox.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::clients::cancel::CancelToken;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::Gremlin;
use crate::executor::run_agent;
use crate::executor::supervisor::chat::chat_payload;
use crate::executor::RunError;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::EventKind;

use crate::executor::supervisor::actor::ActorMsg;
use crate::executor::supervisor::actor_client::Client;

/// Handle returned by the task that runs a chat stage, carrying the stage
/// result and the gremlin back to the forwarding task.
pub(crate) type RunTaskHandle = JoinHandle<(Result<(), RunError>, Gremlin)>;

/// Owns the terminal-path resources for one chat turn.
///
/// Holding these together keeps the "re-insert the gremlin, clear the
/// in-flight turn, publish `done`, write the terminal event" dance in one
/// place instead of repeating it at every exit path.
pub(crate) struct TurnFinalizer {
    id: String,
    turn: Option<String>,
    turn_cancel: Arc<CancelToken>,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
    session_id: String,
    done: Arc<watch::Sender<bool>>,
    writer: SharedWriter,
    /// Bus subscription guard: dropped before the gremlin is re-inserted so
    /// the next turn's events don't fill the old, undrained mailbox.
    _stream_guard: Mutex<Option<BusSubGuard>>,
}

impl TurnFinalizer {
    /// Build a finalizer from the shared turn context, the connection
    /// writer, and the bus subscription guard. Every field is cloned so the
    /// finalizer owns its own copies of the actor mailbox / cancel / done
    /// triple.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor_tx: &mpsc::UnboundedSender<ActorMsg>,
        session_id: &str,
        id: &str,
        turn: &Option<String>,
        turn_cancel: &Arc<CancelToken>,
        done: &Arc<watch::Sender<bool>>,
        writer: SharedWriter,
        stream_guard: BusSubGuard,
    ) -> Self {
        Self {
            id: id.to_string(),
            turn: turn.clone(),
            turn_cancel: Arc::clone(turn_cancel),
            actor_tx: actor_tx.clone(),
            session_id: session_id.to_string(),
            done: Arc::clone(done),
            writer,
            _stream_guard: Mutex::new(Some(stream_guard)),
        }
    }

    /// Clear `current_turn` on the session (only when it still points at this
    /// turn's cancel token), then publish `true` on `done` so waiters proceed.
    pub(crate) async fn clear_turn_and_signal(&self) {
        Client::new(self.actor_tx.clone())
            .session_clear_current_turn(&self.session_id, Arc::clone(&self.turn_cancel))
            .await;
        let _ = self.done.send(true);
    }

    /// Drop the bus subscription guard so the next turn (which reuses
    /// the same run id) gets its own fresh subscription instead of
    /// filling this turn's undrained mailbox.
    fn drop_guard(&self) {
        drop(self._stream_guard.lock().unwrap().take());
    }

    /// Re-insert the gremlin into the session. Drops the bus guard first
    /// so the next turn's events don't queue into this turn's mailbox.
    pub(crate) async fn reinsert(&self, gremlin: Gremlin) {
        self.drop_guard();
        Client::new(self.actor_tx.clone())
            .session_reinsert_gremlin(&self.session_id, &self.id, gremlin)
            .await;
    }

    /// Clear the per-turn conversation artifact when a turn number is known.
    pub(crate) async fn clear_artifact(&self, gremlin: &Gremlin, turn_num: Option<usize>) {
        if let Some(turn_num) = turn_num {
            run_agent::clear_chat_turn_artifact(gremlin, turn_num).await;
        }
    }

    /// Write a chat payload to the connection, tagged with this turn's id.
    pub(crate) async fn write(&self, payload: Value) {
        let payload = chat_payload(&self.turn, payload);
        let _ = self.writer.write_json_line(&payload).await;
    }

    /// Await the run task, re-insert the gremlin if the task returned one,
    /// then clear the in-flight turn and signal completion.
    pub(crate) async fn finish(&self, run_task: &mut RunTaskHandle) {
        match run_task.await {
            Ok((_, gremlin)) => {
                self.reinsert(gremlin).await;
            }
            Err(e) => {
                log::warn!("handle_chat: run task panicked for {}: {e}", self.id);
            }
        }
        self.clear_turn_and_signal().await;
    }

    /// Terminal path for a stream terminal event: finish the turn, then write
    /// the payload (`done` / `ended` / `error`).
    pub(crate) async fn stream_terminal(&self, run_task: &mut RunTaskHandle, payload: Value) {
        self.finish(run_task).await;
        self.write(payload).await;
    }

    /// Cancel the turn: clear the per-turn artifact (when we have a gremlin
    /// and a turn number), re-insert the gremlin when present, clear the
    /// in-flight turn + signal, and emit `ended`.
    ///
    /// When `soft` is true, the caller is signalling a soft-interrupt:
    /// `ChatDefinition` is marked interrupted so the next turn prepends
    /// an interrupt notice, the per-turn artifact is preserved (not cleared),
    /// and the terminal reason is `"interrupted"`.
    pub(crate) async fn cancel(
        &self,
        mut gremlin_opt: Option<Gremlin>,
        chat_turn_num: Option<usize>,
        soft: bool,
    ) {
        if soft {
            if let Some(ref mut gremlin) = gremlin_opt {
                if let Some(chat) = gremlin
                    .definition
                    .as_any_mut()
                    .and_then(|a| a.downcast_mut::<ChatDefinition>())
                {
                    chat.mark_interrupted();
                }
            }
            self.cancel_inner(gremlin_opt, chat_turn_num, false, "interrupted".to_string())
                .await;
        } else {
            self.cancel_inner(gremlin_opt, chat_turn_num, true, "cancelled".to_string())
                .await;
        }
    }

    /// Shared implementation for [`cancel`] and [`task_fallback`].
    async fn cancel_inner(
        &self,
        gremlin_opt: Option<Gremlin>,
        chat_turn_num: Option<usize>,
        clear_artifact_flag: bool,
        reason: String,
    ) {
        if let Some(gremlin) = gremlin_opt {
            if clear_artifact_flag {
                self.clear_artifact(&gremlin, chat_turn_num).await;
            }
            self.reinsert(gremlin).await;
        }
        self.clear_turn_and_signal().await;
        self.write(serde_json::to_value(EventKind::Ended { reason }).unwrap_or_default())
            .await;
    }

    /// Fallback path when the run task finished without a terminal stream
    /// event: clear the artifact, re-insert the gremlin when we have one,
    /// clear the in-flight turn + signal, and emit `ended` with `message`.
    pub(crate) async fn task_fallback(
        &self,
        gremlin_opt: Option<Gremlin>,
        message: String,
        chat_turn_num: Option<usize>,
    ) {
        self.cancel_inner(gremlin_opt, chat_turn_num, true, message)
            .await;
    }
}
