//! State-layer-neutral type for a session's persistent chat-gremlin task.
//!
//! [`ChatGremlinHandle`] lives outside the `chat` module tree so the actor
//! (which stores it on `SessionState`) can depend on it without creating an
//! actor ↔ chat dependency cycle. The per-turn [`super::prompt_msg::PromptMsg`] the
//! handle's channel carries lives in a neutral leaf module at the
//! supervisor level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::clients::cancel::CancelToken;

use super::prompt_msg::PromptMsg;

/// A handle onto a session's persistent chat-gremlin task.
///
/// Cloning is cheap: the prompt sender, the cancel token, and the finished
/// flag are all `Clone`.
#[derive(Clone)]
pub(crate) struct ChatGremlinHandle {
    /// The gremlin id the task drives. A reused gremlin must carry the same
    /// id, so the handle is only reused while the parked gremlin matches.
    pub(crate) id: String,
    /// The bounded(1) prompt channel. A second prompt naturally blocks once a
    /// turn is in flight, serializing turns without any extra locking.
    pub(crate) prompt_tx: mpsc::Sender<PromptMsg>,
    /// Persistent cancel token for the task — a permanent stop.
    pub(crate) cancel: Arc<CancelToken>,
    /// Set by the task itself right before it exits. This is the authoritative
    /// liveness signal: it flips even while a handle clone (e.g. the one stored
    /// on the session) keeps the prompt sender alive, unlike
    /// [`mpsc::Sender::is_closed`] which would stay false until the sender is
    /// dropped.
    pub(crate) finished: Arc<AtomicBool>,
}

impl ChatGremlinHandle {
    /// Whether the task has exited or permanently relinquished its session chat.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed) || self.cancel.is_cancelled()
    }

    /// Queue a turn, returning `false` when the task is gone (closed channel).
    ///
    /// A `try_send` is attempted first so an idle task never blocks the
    /// session reader; when the channel is full the send awaits, matching the
    /// "a second prompt waits for the in-flight turn" behavior.
    pub(crate) async fn send(&self, msg: PromptMsg) -> bool {
        match self.prompt_tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(msg)) => self.prompt_tx.send(msg).await.is_ok(),
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}
