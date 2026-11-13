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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use rig_core::completion::Message;

use crate::clients::cancel::CancelToken;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::Gremlin;
use crate::executor::run_agent;
use crate::executor::supervisor::chat::chat_payload;
use crate::executor::supervisor::SharedWriter;
use crate::executor::RunError;

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
    soft_cancel: Arc<AtomicBool>,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
    session_id: String,
    done: Arc<watch::Sender<bool>>,
    writer: SharedWriter,
}

impl TurnFinalizer {
    /// Build a finalizer from the shared turn context and the connection
    /// writer. Every field is cloned out of `ctx` so the finalizer owns its
    /// own copies of the actor mailbox / cancel / done triple.
    pub(crate) fn new(ctx: &super::TurnCtx, writer: SharedWriter) -> Self {
        Self {
            id: ctx.id.clone(),
            turn: ctx.turn.clone(),
            turn_cancel: Arc::clone(&ctx.turn_cancel),
            soft_cancel: Arc::clone(&ctx.soft_cancel),
            actor_tx: ctx.actor_tx.clone(),
            session_id: ctx.session_id.clone(),
            done: Arc::clone(&ctx.done),
            writer,
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

    /// Re-insert the gremlin into the session.
    pub(crate) async fn reinsert(&self, gremlin: Gremlin) {
        Client::new(self.actor_tx.clone())
            .session_reinsert_gremlin(&self.session_id, &self.id, gremlin)
            .await;
    }

    /// Re-insert the gremlin when present, then clear the in-flight turn and
    /// signal completion.
    pub(crate) async fn reinsert_and_clear(&self, gremlin: Option<Gremlin>) {
        if let Some(gremlin) = gremlin {
            self.reinsert(gremlin).await;
        }
        self.clear_turn_and_signal().await;
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

    /// Hard cancel: clear the per-turn artifact, re-insert the gremlin when we
    /// have one, clear the in-flight turn + signal, and emit `ended`.
    pub(crate) async fn hard_cancel(
        &self,
        gremlin_opt: Option<Gremlin>,
        chat_turn_num: Option<usize>,
    ) {
        if let Some(gremlin) = gremlin_opt {
            self.clear_artifact(&gremlin, chat_turn_num).await;
            self.reinsert(gremlin).await;
        }
        self.clear_turn_and_signal().await;
        self.write(serde_json::json!({
            "type": "ended",
            "id": self.id,
            "reason": "cancelled",
        }))
        .await;
    }

    /// Soft-cancel recovery: persist the partial turn's messages, mark the
    /// definition interrupted, re-insert the gremlin while keeping
    /// `current_turn` alive (marked interrupted), signal completion, and emit
    /// `ended` with reason `interrupted`.
    ///
    /// A hard cancel may race the soft path at two points before the final
    /// section; both are re-checked.
    pub(crate) async fn soft_interrupt(
        &self,
        mut recovered: Gremlin,
        messages: Option<Vec<Message>>,
        pre_turn_len: usize,
        user_prompt: String,
        chat_turn_num: Option<usize>,
    ) {
        // Race guard: a hard cancel may have arrived while we were awaiting
        // the run task. If hard won, discard the partial turn.
        if !self.soft_cancel.load(Ordering::Acquire) {
            self.hard_cancel(Some(recovered), chat_turn_num).await;
            return;
        }

        // Persist partial messages to the current turn's per-turn artifact so
        // the conversation history is preserved. Split off only messages
        // added during this turn to avoid duplicating prior-turn history.
        let mut partial = messages.unwrap_or_default();
        if partial.len() >= pre_turn_len {
            partial = partial.split_off(pre_turn_len);
        } else {
            partial = Vec::new();
        }
        run_agent::strip_unpaired_tool_messages(&mut partial);
        if partial.is_empty() {
            partial.push(Message::user(user_prompt.clone()));
        }
        if let Some(turn_num) = chat_turn_num {
            run_agent::append_chat_conversation_turn(&recovered, turn_num, &partial).await;
        }

        // Race guard: a hard cancel may have arrived during
        // `append_chat_conversation_turn`. If hard won, clear the artifact and
        // skip `mark_interrupted()`.
        if !self.soft_cancel.load(Ordering::Acquire) {
            self.hard_cancel(Some(recovered), chat_turn_num).await;
            return;
        }

        // Mark the definition as interrupted so the next turn injects the
        // interrupt notice.
        if let Some(chat) = recovered
            .definition
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ChatDefinition>())
        {
            chat.mark_interrupted();
        }

        // Re-insert the gremlin, then mark `current_turn` interrupted. The
        // actor guards the interrupted flip by the turn's cancel token, so a
        // hard cancel that won the race is not mis-marked.
        self.reinsert_and_mark_interrupted(recovered, chat_turn_num)
            .await;
    }

    /// Re-insert the gremlin and mark `current_turn.interrupted`. If hard
    /// cancellation has won, falls back to [`Self::hard_cancel`] instead.
    async fn reinsert_and_mark_interrupted(&self, gremlin: Gremlin, chat_turn_num: Option<usize>) {
        if !self.soft_cancel.load(Ordering::Acquire) {
            // Hard cancel won while we were re-inserting.
            self.hard_cancel(Some(gremlin), chat_turn_num).await;
            return;
        }
        Client::new(self.actor_tx.clone())
            .session_reinsert_and_mark_interrupted(
                &self.session_id,
                &self.id,
                gremlin,
                Arc::clone(&self.turn_cancel),
            )
            .await;
        let _ = self.done.send(true);
        self.write(serde_json::json!({
            "type": "ended",
            "id": self.id,
            "reason": "interrupted",
        }))
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
        if let Some(gremlin) = gremlin_opt {
            self.clear_artifact(&gremlin, chat_turn_num).await;
            self.reinsert(gremlin).await;
        }
        self.clear_turn_and_signal().await;
        self.write(serde_json::json!({
            "type": "ended",
            "id": self.id,
            "reason": message,
        }))
        .await;
    }
}
