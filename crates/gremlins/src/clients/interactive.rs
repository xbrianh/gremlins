//! The run handle publishes pause requests; only the loop that accepts a
//! request receives that attachment's commands. Stream events stay on the bus.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::clients::protocol::UsageStats;
use tokio::sync::{mpsc, oneshot, Notify};

/// The terminal debug frames have different wire shapes. Never synthesize a
/// successful model turn from a command-channel close or cancellation.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DebugOutcome {
    Done { text: String, usage: UsageStats },
    Ended { reason: String },
}

/// Reason string for [`DebugOutcome::Ended`] when a [`InteractiveCommand::Message`]
/// correction is delivered.  Emitted by `interactive_loop`, consumed by
/// `turn_runner`.
pub(crate) const MESSAGE_DELIVERED_REASON: &str = "message delivered";

#[derive(Debug)]
pub(crate) enum InteractiveCommand {
    /// Inject text and run a single step with a debug-amended system prompt.
    Inject(String),
    /// Deliver a correction, release the pause slot, and resume the agent loop.
    /// Does NOT amend the system prompt — keeps the original model/prompt.
    Message(String),
    /// Run a single turn without injecting text (operator 'continue').
    RunTurn,
    /// Terminate the agent with an error.
    Bail(String),
    /// Resume autonomous execution (detach the debugger).
    Quit,
}

#[derive(Debug)]
struct CommandInbox {
    commands: mpsc::Receiver<InteractiveCommand>,
    ready: Option<oneshot::Sender<()>>,
    ended: Option<oneshot::Sender<DebugOutcome>>,
}

/// Outlives the command inbox so a supervisor can publish the actual turn
/// outcome after the agent unwinds. Only an accepted attachment registers.
#[derive(Clone, Debug, Default)]
pub(crate) struct DebugTerminal(Arc<Mutex<Option<oneshot::Sender<DebugOutcome>>>>);

impl DebugTerminal {
    fn finish(&self, outcome: DebugOutcome) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(outcome);
        }
    }

    pub(crate) fn end(&self, reason: &str) {
        self.finish(DebugOutcome::Ended {
            reason: reason.into(),
        });
    }
}

#[derive(Debug, Default)]
enum PauseState {
    #[default]
    Idle,
    /// A debugger has placed a pause request. The u64 is the execution
    /// nonce under which the request was made; it lets [`next_session`]
    /// distinguish a between-turn request from a stale leftover.
    Requested(CommandInbox, u64),
    /// The agent loop has accepted the pause. The u64 records the
    /// execution nonce at the time of acceptance.
    Accepted(u64),
}

impl PauseState {
    fn accept(&mut self) -> Option<CommandInbox> {
        match std::mem::replace(self, Self::Idle) {
            Self::Requested(inbox, nonce) => {
                *self = Self::Accepted(nonce);
                Some(inbox)
            }
            previous => {
                *self = previous;
                None
            }
        }
    }
}

#[derive(Debug, Default)]
struct PauseSlot {
    state: Mutex<PauseState>,
    requested: Notify,
    /// Identifies one execution (one agent loop invocation). Incremented when
    /// a new session is minted from this slot.
    nonce: AtomicU64,
}

/// The published run handle points to one execution at a time. A debugger
/// snapshots its target *before* validating the active leaf against the store.
#[derive(Clone, Debug, Default)]
pub(crate) struct InteractiveHandle(Arc<Mutex<DebugTarget>>);

impl InteractiveHandle {
    pub(crate) fn target(&self) -> DebugTarget {
        self.0.lock().unwrap().clone()
    }

    fn publish(&self, slot: &Arc<PauseSlot>) {
        *self.0.lock().unwrap() = DebugTarget {
            slot: Arc::downgrade(slot),
            nonce: slot.nonce.load(Ordering::Relaxed),
        };
    }

    #[cfg(test)]
    pub(crate) fn attach(&self) -> Option<DebugAttachment> {
        self.target().attach().ok()
    }
}

/// Weak by design: neither validation nor a silent debugger keeps a static
/// execution's pending request alive after its agent session goes away.
#[derive(Clone, Debug, Default)]
pub(crate) struct DebugTarget {
    slot: Weak<PauseSlot>,
    nonce: u64,
}

impl DebugTarget {
    /// The execution identity captured when the target was published.
    pub(crate) fn nonce(&self) -> u64 {
        self.nonce
    }

    pub(crate) fn attach(&self) -> Result<DebugAttachment, &'static str> {
        let slot = self
            .slot
            .upgrade()
            .ok_or("agent stage is no longer active")?;
        if slot.nonce.load(Ordering::Relaxed) != self.nonce {
            return Err("execution has been replaced; target is stale");
        }
        let mut state = slot.state.lock().unwrap();
        if !matches!(*state, PauseState::Idle) {
            return Err("debugger already attached");
        }
        let (commands, rx) = mpsc::channel(8);
        let (tx, ready) = oneshot::channel();
        let (terminal, ended) = oneshot::channel();
        *state = PauseState::Requested(
            CommandInbox {
                commands: rx,
                ready: Some(tx),
                ended: Some(terminal),
            },
            self.nonce,
        );
        slot.requested.notify_one();
        Ok(DebugAttachment {
            slot: self.slot.clone(),
            commands,
            ready,
            ended,
        })
    }
}

/// Withdrawing a pending pause and accepting it are mutually exclusive.
/// If already accepted, dropping the sole sender wakes just that receiver.
#[derive(Debug)]
pub(crate) struct DebugAttachment {
    slot: Weak<PauseSlot>,
    commands: mpsc::Sender<InteractiveCommand>,
    pub(crate) ready: oneshot::Receiver<()>,
    pub(crate) ended: oneshot::Receiver<DebugOutcome>,
}

impl Drop for DebugAttachment {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.upgrade() {
            *slot.state.lock().unwrap_or_else(|e| e.into_inner()) = PauseState::Idle;
        }
    }
}

impl DebugAttachment {
    pub(crate) fn try_send(
        &self,
        command: InteractiveCommand,
    ) -> Result<(), mpsc::error::TrySendError<InteractiveCommand>> {
        self.commands.try_send(command)
    }

    #[cfg(test)]
    pub(crate) async fn send(
        &self,
        command: InteractiveCommand,
    ) -> Result<(), mpsc::error::SendError<InteractiveCommand>> {
        self.commands.send(command).await
    }
}

#[derive(Debug)]
pub(crate) struct InteractiveSessionFactory {
    handle: InteractiveHandle,
    /// Only chats allow a pending request to cross the gap between turns.
    chat_slot: Option<Arc<PauseSlot>>,
    next_terminal: Option<DebugTerminal>,
}

impl InteractiveSessionFactory {
    pub(crate) fn for_pipeline() -> (Self, InteractiveHandle) {
        Self::with_chat_slot(None)
    }

    pub(crate) fn for_chat() -> (Self, InteractiveHandle) {
        Self::with_chat_slot(Some(Arc::default()))
    }

    fn with_chat_slot(chat_slot: Option<Arc<PauseSlot>>) -> (Self, InteractiveHandle) {
        let handle = InteractiveHandle::default();
        if let Some(slot) = &chat_slot {
            handle.publish(slot);
        }
        (
            Self {
                handle: handle.clone(),
                chat_slot,
                next_terminal: None,
            },
            handle,
        )
    }

    /// Chat classifies soft vs. hard cancellation after joining the stage.
    pub(crate) fn supervise_next_turn(&mut self) -> DebugTerminal {
        self.next_terminal.insert(DebugTerminal::default()).clone()
    }

    pub(crate) fn next_session(&mut self) -> InteractiveSession {
        let slot = self.chat_slot.clone().unwrap_or_default();
        // Mint a fresh execution identity *before* sweeping stale state
        // so that the sweep is atomic with respect to the new nonce — a
        // between-turn debugger that has not yet attached sees the new
        // nonce and validates against it, while a request that was
        // already placed under the old nonce is recognised as a
        // between-turn holdover and updated.
        let nonce = slot.nonce.fetch_add(1, Ordering::Relaxed) + 1;
        self.handle.publish(&slot);
        if self.chat_slot.is_some() {
            let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*state {
                // Stale acceptance left over from the previous execution —
                // that execution's agent loop is gone, so no one will
                // consume the pause.  Reset it so a new debugger can attach.
                PauseState::Accepted(old_nonce) if *old_nonce != nonce => {
                    *state = PauseState::Idle;
                }
                // Request placed between turns under the previous nonce.
                // Carry it forward to the new execution so the between-turn
                // debugger's pause-on-entry is honoured.
                PauseState::Requested(_, old_nonce) if *old_nonce != nonce => {
                    if let PauseState::Requested(inbox, _) =
                        std::mem::replace(&mut *state, PauseState::Idle)
                    {
                        *state = PauseState::Requested(inbox, nonce);
                    }
                }
                _ => {}
            }
        }
        InteractiveSession {
            slot,
            nonce,
            active: None,
            terminal: self.next_terminal.take(),
        }
    }
}

#[derive(Debug, Default)]
pub struct InteractiveSession {
    slot: Arc<PauseSlot>,
    /// Identifies this execution so commands can be checked against the
    /// correct agent loop invocation.
    #[allow(dead_code)]
    nonce: u64,
    active: Option<CommandInbox>,
    terminal: Option<DebugTerminal>,
}

impl InteractiveSession {
    pub fn unattached() -> Self {
        Self::default()
    }

    /// Without an outer chat supervisor the agent reports its own result.
    pub(crate) fn supervise_if_needed(&mut self) -> Option<DebugTerminal> {
        if self.terminal.is_some() {
            None
        } else {
            Some(self.terminal.insert(DebugTerminal::default()).clone())
        }
    }

    pub(crate) fn accept_pause(&mut self) -> bool {
        let Some(mut inbox) = self.slot.state.lock().unwrap().accept() else {
            return false;
        };
        if let Some(terminal) = &self.terminal {
            *terminal.0.lock().unwrap() = inbox.ended.take();
        }
        self.active = Some(inbox);
        true
    }

    pub(crate) async fn paused(&mut self) {
        while !self.accept_pause() {
            // notify_one stores a permit even if attach races this await.
            self.slot.requested.notified().await;
        }
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn finish_attachment(&mut self, outcome: DebugOutcome) {
        if let Some(terminal) = &self.terminal {
            terminal.finish(outcome);
        } else if let Some(tx) = self.active.as_mut().and_then(|inbox| inbox.ended.take()) {
            let _ = tx.send(outcome);
        }
        self.active = None;
    }

    /// Called after Ready has been emitted, including when re-entering after
    /// a single step. Only the first Ready completes the socket handshake.
    pub(crate) async fn next_command(&mut self) -> Option<InteractiveCommand> {
        let inbox = self.active.as_mut().expect("accepted pause");
        if let Some(ready) = inbox.ready.take() {
            let _ = ready.send(());
        }
        inbox.commands.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn static_pause_expires_with_the_leaf_that_was_validated() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_pipeline();
        let first = sessions.next_session();
        let validated = handle.target();
        let mut pending = validated.attach().unwrap();

        // The leaf finishes without polling the request. Neither a pending
        // handshake nor validation in progress can follow the published handle.
        drop(first);
        let mut second = sessions.next_session();
        assert!((&mut pending.ready).now_or_never().unwrap().is_err());
        assert!(validated.attach().is_err());
        assert!(!second.accept_pause());

        let current = handle.target().attach().unwrap();
        drop(pending);
        assert!(second.accept_pause());
        current.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            second.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
    }

    #[tokio::test]
    async fn supervisor_outcome_outlives_the_accepted_command_inbox() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let terminal = sessions.supervise_next_turn();
        let mut session = sessions.next_session();
        let mut debugger = handle.attach().unwrap();
        session.paused().await;
        assert!(session.next_command().now_or_never().is_none());
        (&mut debugger.ready).await.unwrap();
        drop(session);
        assert!((&mut debugger.ended).now_or_never().is_none());
        terminal.end("interrupted");
        assert!(matches!(
            (&mut debugger.ended).await,
            Ok(DebugOutcome::Ended { reason }) if reason == "interrupted"
        ));
    }

    #[tokio::test]
    async fn dropping_an_attachment_only_releases_its_own_pause() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut first = sessions.next_session();
        let old = handle.attach().unwrap();
        first.paused().await;
        assert!(handle.attach().is_none());
        drop(old);

        let next = handle.attach().unwrap();
        let mut second = sessions.next_session();
        second.paused().await;
        assert!(first.next_command().await.is_none());
        assert!(second.next_command().now_or_never().is_none());
        next.send(InteractiveCommand::RunTurn).await.unwrap();
        assert!(matches!(
            second.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
    }
}
