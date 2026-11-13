//! Interactive operator prompt for agent stages.
//!
//! Channels created once per gremlin at launch time. The supervisor holds an
//! [`InteractiveHandle`]; the agent loop receives an [`InteractiveSession`].
//! Stream events flow through the event bus, not through these channels.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};

/// A resettable signal that interrupts the agent loop at any async yield point.
/// Modelled on [`CancelToken`] but with a public `reset()` so the agent can
/// resume normal operation after a debug session ends.
#[derive(Debug)]
pub struct PauseToken {
    flag: AtomicBool,
    notify: Notify,
}

impl PauseToken {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    /// Signal the agent loop to pause at the next yield point.
    ///
    /// Uses [`Notify::notify_one`] rather than `notify_waiters` so the
    /// permit is stored when no waiter is registered yet — eliminating the
    /// race between flag check and `notified().await` in [`paused`](Self::paused).
    pub fn pause(&self) {
        log::debug!("PauseToken::pause() called");
        self.flag.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Clear the pause signal so the agent can resume normal operation.
    pub fn reset(&self) {
        log::debug!("PauseToken::reset() called");
        self.flag.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        let paused = self.flag.load(Ordering::Acquire);
        if paused {
            log::debug!("PauseToken::is_paused() -> true");
        }
        paused
    }

    /// Future that resolves when `pause()` is called.
    ///
    /// Safe against the race where `pause()` fires between the flag check
    /// and `notified().await`: `pause()` uses `notify_one`, which stores a
    /// permit when no waiter is registered, so the next `notified().await`
    /// completes immediately.
    ///
    /// After waking, re-checks the flag so that a `reset()` between
    /// `pause()` and `paused()` does not cause a spurious resolution.
    pub async fn paused(&self) {
        log::debug!("PauseToken::paused() — entering wait loop");
        loop {
            let notified = self.notify.notified();
            if self.flag.load(Ordering::Acquire) {
                log::debug!("PauseToken::paused() — flag already set, returning immediately");
                return;
            }
            log::debug!("PauseToken::paused() — waiting for notify");
            notified.await;
            // Woke up — re-check the flag before returning.
            // If reset() was called, the flag is false and we loop.
            log::debug!("PauseToken::paused() — notified, re-checking flag");
        }
    }
}

/// Commands the supervisor sends to the agent loop over an mpsc channel.
///
/// `Pause` is no longer carried on this channel — use [`PauseToken::pause`]
/// instead for immediate interrupt at any yield point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InteractiveCommand {
    /// Inject an operator message and run one turn.
    Inject(String),
    /// Run one turn with the current next_prompt, then re-pause.
    RunTurn,
    /// Terminate the run with a bail reason.
    Bail(String),
    /// Exit interactive mode and resume normal operation.
    Quit,
}

// ── Channel pair ──────────────────────────────────────────────────────────

/// Created once per gremlin at launch. Call [`split`](InteractiveChannels::split)
/// to produce the supervisor-side [`InteractiveHandle`] and the agent-side
/// [`InteractiveSession`].
pub(crate) struct InteractiveChannels {
    pub cmd_tx: mpsc::Sender<InteractiveCommand>,
    pub cmd_rx: mpsc::Receiver<InteractiveCommand>,
    pub pause: Arc<PauseToken>,
}

impl InteractiveChannels {
    /// Create a new channel pair with a shared [`PauseToken`].
    pub(crate) fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let pause = PauseToken::new();
        Self {
            cmd_tx,
            cmd_rx,
            pause,
        }
    }

    /// Split into the supervisor-side handle and the agent-side session.
    /// Both halves share the same pause token.
    pub(crate) fn split(self) -> (InteractiveHandle, InteractiveSession) {
        let handle = InteractiveHandle {
            cmd_tx: self.cmd_tx,
            pause: self.pause.clone(),
        };
        let session = InteractiveSession {
            cmd_rx: self.cmd_rx,
            pause: self.pause,
        };
        (handle, session)
    }
}

impl Default for InteractiveChannels {
    fn default() -> Self {
        Self::new()
    }
}

// ── Supervisor-side handle ────────────────────────────────────────────────

/// Stored in [`RunHandle`] and [`RuntimeConfig`]. The supervisor uses this to
/// send commands and trigger pause. Stream events are consumed via the event
/// bus, not this handle.
#[derive(Clone, Debug)]
pub(crate) struct InteractiveHandle {
    pub cmd_tx: mpsc::Sender<InteractiveCommand>,
    pub pause: Arc<PauseToken>,
}

// ── Agent-side session ────────────────────────────────────────────────────

/// Threaded through [`RunParams`] into the agent loop. The agent loop reads
/// commands from `cmd_rx` and watches `pause` for immediate interrupt
/// signals. Stream events are emitted through the agent loop's `LoopHooks`
/// rather than this session.
#[derive(Debug)]
pub struct InteractiveSession {
    pub cmd_rx: mpsc::Receiver<InteractiveCommand>,
    pub pause: Arc<PauseToken>,
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// pause() notifies a waiter in paused() — even when pause() fires
    /// before paused() is awaited (the notify_one permit is stored).
    #[tokio::test]
    async fn pause_token_wakes_waiter() {
        let token = PauseToken::new();
        assert!(!token.is_paused());

        // Fire pause before the waiter starts — the permit must be stored.
        token.pause();
        assert!(token.is_paused());

        // The waiter should resolve immediately.
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            token.paused().await;
            true
        })
        .await;
        assert!(result.is_ok(), "paused() timed out");
        assert!(result.unwrap());
    }

    /// reset() clears the flag so is_paused() returns false.
    #[tokio::test]
    async fn pause_token_reset_clears_flag() {
        let token = PauseToken::new();
        token.pause();
        assert!(token.is_paused());
        token.reset();
        assert!(!token.is_paused());
    }

    /// reset() between pause() and paused() — the notify permit was stored,
    /// so paused() still resolves, but the flag check at the top of the
    /// loop sees false and loops back.
    #[tokio::test]
    async fn pause_then_reset_before_await() {
        let token = PauseToken::new();
        token.pause();
        token.reset();

        // paused() should NOT resolve within a short timeout because the
        // flag is false — it waits for another notify.
        let result = tokio::time::timeout(Duration::from_millis(200), async {
            token.paused().await;
            true
        })
        .await;
        assert!(
            result.is_err(),
            "paused() should have timed out after reset()"
        );

        // Now pause again — the waiting task should resolve.
        token.pause();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            token.paused().await;
            true
        })
        .await;
        assert!(result.is_ok(), "paused() timed out after second pause()");
        assert!(result.unwrap());
    }

    /// Split channels: send a command from handle side, receive on session side.
    #[tokio::test]
    async fn channels_deliver_commands() {
        let channels = InteractiveChannels::new();
        let (handle, mut session) = channels.split();

        // Send Inject from handle side.
        handle
            .cmd_tx
            .send(InteractiveCommand::Inject("hello".to_string()))
            .await
            .unwrap();

        // Receive on session side.
        let cmd = session.cmd_rx.recv().await.unwrap();
        assert!(matches!(cmd, InteractiveCommand::Inject(ref text) if text == "hello"));
    }

    /// When cmd_rx is dropped, recv() on the session side returns None.
    #[tokio::test]
    async fn cmd_rx_closed_when_handle_dropped() {
        let channels = InteractiveChannels::new();
        let (handle, mut session) = channels.split();

        // Drop the handle side — cmd_tx is dropped, so cmd_rx should close.
        drop(handle);

        let cmd = session.cmd_rx.recv().await;
        assert!(
            cmd.is_none(),
            "cmd_rx should return None when sender is dropped"
        );
    }

    /// When the agent-side session is dropped, the handle's cmd_tx.closed()
    /// future resolves — letting the supervisor detect a dead agent.
    #[tokio::test]
    async fn cmd_tx_closed_when_session_dropped() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        let cmd_tx_closed = handle.cmd_tx.closed();
        tokio::pin!(cmd_tx_closed);

        // Before dropping the session, cmd_tx should not be closed.
        // (We can't assert it's not ready without a race, but we assert
        // that after drop it resolves promptly.)
        drop(session);

        let result = tokio::time::timeout(Duration::from_secs(2), &mut cmd_tx_closed).await;
        assert!(
            result.is_ok(),
            "cmd_tx.closed() should resolve after session is dropped"
        );
    }
}
