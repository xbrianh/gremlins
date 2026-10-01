//! Interactive operator prompt for agent stages.
//!
//! Channel triplet created once per gremlin at launch time. The supervisor
//! holds an [`InteractiveHandle`]; the agent loop receives an
//! [`InteractiveSession`]. Both share the same broadcast event sender.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Notify};

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
        self.flag.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.flag.load(Ordering::Acquire)
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

/// Events the agent loop broadcasts back to the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InteractiveEvent {
    /// Agent has entered interactive mode and is ready for commands.
    Ready {
        /// Current turn number.
        turn: usize,
    },
    /// A turn (injected or run-turn) completed; agent is waiting.
    TurnComplete {
        /// Turn number that just completed.
        turn: usize,
        /// Assistant text from the completed turn.
        text: String,
        /// Tool calls from the completed turn.
        tool_calls: Vec<String>,
    },
    /// Agent called Done while interactive.
    Done {
        /// Final text result.
        text: String,
        /// Token usage summary.
        usage: Option<super::protocol::UsageStats>,
    },
    /// Interactive session ended.
    Ended {
        /// How the session ended: "resumed", "bailed", or "disconnect".
        reason: String,
    },
}

// ── Channel triplet ───────────────────────────────────────────────────────

/// Created once per gremlin at launch. Call [`split`](InteractiveChannels::split)
/// to produce the supervisor-side [`InteractiveHandle`] and the agent-side
/// [`InteractiveSession`].
pub(crate) struct InteractiveChannels {
    pub cmd_tx: mpsc::Sender<InteractiveCommand>,
    pub cmd_rx: mpsc::Receiver<InteractiveCommand>,
    pub evt_tx: broadcast::Sender<InteractiveEvent>,
    pub pause: Arc<PauseToken>,
}

impl InteractiveChannels {
    /// Create a new channel triplet with a shared [`PauseToken`].
    pub(crate) fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (evt_tx, _) = broadcast::channel(16);
        let pause = PauseToken::new();
        Self {
            cmd_tx,
            cmd_rx,
            evt_tx,
            pause,
        }
    }

    /// Split into the supervisor-side handle and the agent-side session.
    /// Both halves share the same broadcast event sender and pause token.
    pub(crate) fn split(self) -> (InteractiveHandle, InteractiveSession) {
        let handle = InteractiveHandle {
            cmd_tx: self.cmd_tx,
            evt_tx: self.evt_tx.clone(),
            pause: self.pause.clone(),
        };
        let session = InteractiveSession {
            cmd_rx: self.cmd_rx,
            evt_tx: self.evt_tx,
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
/// send commands, trigger pause, and subscribe to events.
#[derive(Clone, Debug)]
pub(crate) struct InteractiveHandle {
    pub cmd_tx: mpsc::Sender<InteractiveCommand>,
    pub evt_tx: broadcast::Sender<InteractiveEvent>,
    pub pause: Arc<PauseToken>,
}

// ── Agent-side session ────────────────────────────────────────────────────

/// Threaded through [`RunParams`] into the agent loop. The agent loop reads
/// commands from `cmd_rx`, broadcasts events on `evt_tx`, and watches
/// `pause` for immediate interrupt signals.
#[derive(Debug)]
pub struct InteractiveSession {
    pub cmd_rx: mpsc::Receiver<InteractiveCommand>,
    pub evt_tx: broadcast::Sender<InteractiveEvent>,
    pub pause: Arc<PauseToken>,
}
