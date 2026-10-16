//! Interactive operator prompt for agent stages.
//!
//! Channel triplet created once per gremlin at launch time. The supervisor
//! holds an [`InteractiveHandle`]; the agent loop receives an
//! [`InteractiveSession`]. Both share the same broadcast event sender.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Notify};

use super::protocol::{TurnStats, UsageStats};

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
        /// Per-turn token/cache delta for the completed turn.
        stats: Option<TurnStats>,
        /// Token/cache totals for the current prompt, including this turn.
        totals: Option<UsageStats>,
    },
    /// Agent called Done while interactive.
    Done {
        /// Final text result.
        text: String,
        /// The `Done` tool's `summary` argument, preserved distinctly from
        /// the streamed text so consumers can append it at turn end.
        summary: String,
        /// Token usage summary.
        usage: Option<super::protocol::UsageStats>,
    },
    /// Incremental text fragment from the model stream.
    StreamChunk { text: String },
    /// Incremental reasoning fragment from the model stream.
    ReasoningChunk { text: String },
    /// A line-buffered tool-output chunk streamed while a tool runs. This is
    /// display-only and never reaches the model or the permanent transcript.
    ToolOutputChunk {
        /// Tool-call id this chunk belongs to.
        id: String,
        name: String,
        /// Formatted tool call (`format_tool_call` output), used by the TUI
        /// as the live tool-slot title row.
        call: String,
        text: String,
    },
    /// A tool result produced after execution.
    ToolResult {
        /// Tool-call id this result belongs to.
        id: String,
        name: String,
        /// Formatted tool call (`format_tool_call` output), used by the TUI
        /// as the committed tool-block title row.
        call: String,
        output: String,
    },
    /// Interactive session ended.
    Ended {
        /// How the session ended: "resumed", "bailed", or "disconnect".
        reason: String,
    },
    /// The current turn's streamed deltas (text / reasoning) must be
    /// discarded because a mid-stream retry is restarting the turn.
    /// Consumers should clear any accumulated partial output for this
    /// turn so they don't concatenate fragments from the failed attempt
    /// with the retried attempt.
    StreamReset,
}

/// Sender for stream events emitted by the agent loop.
///
/// The interactive/debug path uses a broadcast sender because it genuinely
/// has multiple subscribers (the debug supervisor calls `subscribe()`). The
/// chat path uses a bounded mpsc sender so a single runaway chat can never
/// exhaust daemon memory — the bound limits memory, while an awaited send
/// provides backpressure instead of dropping live stream events.
#[derive(Clone, Debug)]
pub enum StreamEventSender {
    Broadcast(broadcast::Sender<InteractiveEvent>),
    Mpsc(mpsc::Sender<InteractiveEvent>),
}

impl StreamEventSender {
    /// Send one stream event.
    ///
    /// The broadcast variant preserves the existing `receiver_count()` gate
    /// and lag-tolerant semantics. The mpsc variant awaits channel capacity,
    /// so a slow chat consumer throttles the producer instead of causing
    /// silent gaps in live text / reasoning / tool output.
    pub(crate) async fn send(&self, event: InteractiveEvent) {
        match self {
            Self::Broadcast(tx) => {
                if tx.receiver_count() > 0 {
                    let _ = tx.send(event);
                }
            }
            Self::Mpsc(tx) => {
                // Await capacity rather than `try_send`-dropping. The chat
                // `run_chat_turn` drain is the sole consumer and is always
                // polled concurrently, so this backpressure never deadlocks;
                // it just makes the agent loop wait for the front-end.
                let _ = tx.send(event).await;
            }
        }
    }

    /// Reserve the right to send one event without awaiting again.
    ///
    /// The mpsc variant waits for channel capacity; the returned permit then
    /// sends synchronously without another await. This lets the live
    /// tool-output forwarder receive a dequeued line only after chat
    /// capacity is already secured, so an interrupting tool batch cannot
    /// drop an in-flight chunk between `recv` and `send`. The broadcast
    /// variant has no capacity to reserve.
    pub(crate) async fn reserve(&self) -> StreamEventPermit<'_> {
        match self {
            Self::Broadcast(_) => StreamEventPermit::Broadcast(self),
            Self::Mpsc(tx) => match tx.reserve().await {
                Ok(permit) => StreamEventPermit::Mpsc(permit),
                Err(_closed) => StreamEventPermit::Closed,
            },
        }
    }

    /// Whether [`send`](Self::send) will actually deliver an event.
    ///
    /// Broadcast senders only deliver when at least one receiver is
    /// subscribed, so callers can skip building an [`InteractiveEvent`]
    /// entirely for unsubscribed broadcast senders. Mpsc senders always
    /// deliver (awaiting capacity when necessary), so they report `true`.
    pub(crate) fn wants_events(&self) -> bool {
        match self {
            Self::Broadcast(tx) => tx.receiver_count() > 0,
            Self::Mpsc(_) => true,
        }
    }
}

/// A pre-reserved stream-channel slot.
pub(crate) enum StreamEventPermit<'a> {
    Broadcast(&'a StreamEventSender),
    Mpsc(mpsc::Permit<'a, InteractiveEvent>),
    Closed,
}

impl StreamEventPermit<'_> {
    /// Send one event using the reserved slot. This never awaits.
    pub(crate) fn send(self, event: InteractiveEvent) {
        match self {
            Self::Broadcast(sender) => {
                if let StreamEventSender::Broadcast(tx) = sender {
                    if tx.receiver_count() > 0 {
                        let _ = tx.send(event);
                    }
                }
            }
            Self::Mpsc(permit) => permit.send(event),
            Self::Closed => {}
        }
    }
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
        // Bumped well above the old 16 so a bursty tool-output stream (now
        // coalesced to one chunk per 8 KiB read) doesn't lag the debug
        // consumer on every Bash call.
        let (evt_tx, _) = broadcast::channel(256);
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

    /// Events broadcast from session side arrive on handle side.
    #[tokio::test]
    async fn events_broadcast_to_subscriber() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        // Subscribe from handle side.
        let mut evt_rx = handle.evt_tx.subscribe();

        // Send from session side.
        let _ = session.evt_tx.send(InteractiveEvent::Ready { turn: 1 });

        // Receive on handle side.
        let evt = evt_rx.recv().await.unwrap();
        assert!(matches!(evt, InteractiveEvent::Ready { turn: 1 }));
    }

    /// Full debug-flow simulation: supervisor pauses agent, agent enters
    /// interactive mode and broadcasts Ready.
    #[tokio::test]
    async fn debug_flow_pause_and_ready() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        // Subscribe on the supervisor side.
        let mut evt_rx = handle.evt_tx.subscribe();

        // Agent side: spawn a task that mimics the agent loop waiting for
        // a pause, then entering interactive mode.
        let agent_handle = tokio::spawn(async move {
            let InteractiveSession {
                mut cmd_rx,
                evt_tx,
                pause,
            } = session;

            // Wait for the pause signal (simulating the agent loop's
            // tokio::select! mid-stream or turn-boundary check).
            pause.paused().await;

            // Enter interactive mode: broadcast Ready.
            let _ = evt_tx.send(InteractiveEvent::Ready { turn: 3 });

            // Wait for a command.
            let cmd = cmd_rx.recv().await.unwrap();
            assert!(matches!(cmd, InteractiveCommand::Quit));

            // Broadcast Ended.
            let _ = evt_tx.send(InteractiveEvent::Ended {
                reason: "resumed".to_string(),
            });
        });

        // Supervisor side: trigger pause.
        handle.pause.pause();

        // Wait for Ready from the agent.
        let evt = tokio::time::timeout(Duration::from_secs(2), evt_rx.recv())
            .await
            .expect("timed out waiting for Ready")
            .expect("evt_rx closed");
        assert!(matches!(evt, InteractiveEvent::Ready { turn: 3 }));

        // Send Quit to the agent.
        handle.cmd_tx.send(InteractiveCommand::Quit).await.unwrap();

        // Wait for Ended.
        let evt = tokio::time::timeout(Duration::from_secs(2), evt_rx.recv())
            .await
            .expect("timed out waiting for Ended")
            .expect("evt_rx closed");
        assert!(matches!(evt, InteractiveEvent::Ended { ref reason } if reason == "resumed"));

        agent_handle.await.unwrap();
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

    /// Multiple subscribers all receive the same event.
    #[tokio::test]
    async fn broadcast_delivers_to_all_subscribers() {
        let channels = InteractiveChannels::new();
        let (handle, session) = channels.split();

        let mut rx1 = handle.evt_tx.subscribe();
        let mut rx2 = handle.evt_tx.subscribe();

        let _ = session.evt_tx.send(InteractiveEvent::Ready { turn: 0 });

        let evt1 = rx1.recv().await.unwrap();
        let evt2 = rx2.recv().await.unwrap();
        assert!(matches!(evt1, InteractiveEvent::Ready { turn: 0 }));
        assert!(matches!(evt2, InteractiveEvent::Ready { turn: 0 }));
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

    /// `wants_events` is false for an unsubscribed broadcast sender and true
    /// for mpsc, so callers can skip building payloads when nobody listens.
    #[test]
    fn stream_event_sender_wants_events() {
        let (broadcast_tx, _) = broadcast::channel(16);
        let broadcast_sender = StreamEventSender::Broadcast(broadcast_tx.clone());
        assert!(!broadcast_sender.wants_events());

        let rx = broadcast_tx.subscribe();
        assert!(broadcast_sender.wants_events());
        drop(rx);
        assert!(!broadcast_sender.wants_events());

        let (mpsc_tx, _mpsc_rx) = mpsc::channel(16);
        let mpsc_sender = StreamEventSender::Mpsc(mpsc_tx);
        assert!(mpsc_sender.wants_events());
    }

    /// A bounded mpsc stream sender blocks on a full channel instead of
    /// dropping, so the chat stream stays lossless under backpressure.
    #[tokio::test]
    async fn mpsc_stream_sender_blocks_on_overflow_then_delivers() {
        let (tx, mut rx) = mpsc::channel::<InteractiveEvent>(16);
        let sender = StreamEventSender::Mpsc(tx);

        // Fill the channel to capacity without blocking.
        for i in 0..16 {
            sender
                .send(InteractiveEvent::StreamChunk {
                    text: format!("chunk-{i}"),
                })
                .await;
        }

        // The 17th send must not resolve until the consumer drains one slot.
        let sender_clone = sender.clone();
        let blocked_send = tokio::spawn(async move {
            sender_clone
                .send(InteractiveEvent::StreamChunk {
                    text: "chunk-blocked".into(),
                })
                .await;
        });
        tokio::task::yield_now().await;
        assert!(
            !blocked_send.is_finished(),
            "mpsc send must block on a full channel instead of dropping"
        );

        // Drain one buffered event; that frees capacity for the blocked send.
        let first = rx.recv().await.expect("first buffered event");
        assert!(matches!(first, InteractiveEvent::StreamChunk { .. }));
        tokio::time::timeout(Duration::from_secs(1), blocked_send)
            .await
            .expect("blocked send should complete once capacity is available")
            .expect("blocked send task should not panic");

        drop(sender);

        // The channel closes after the original sender is dropped and the
        // cloned sender returned from the spawned task. Every queued event
        // (the 15 remaining initial chunks plus the blocked one) survives.
        let mut count = 1; // `first`, already drained above
        while rx.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 17, "every event must be delivered losslessly");
    }
}
