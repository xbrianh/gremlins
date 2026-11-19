//! The run handle publishes pause requests; only the loop that accepts a
//! request receives that attachment's commands. Stream events stay on the bus.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    ///
    /// The second field is the client-supplied correlation id (`req_id`). The
    /// third is a one-shot sender the agent loop fires after
    /// `conv.inject_operator` succeeds, letting the owner await delivery.
    Message(String, Option<String>, Option<oneshot::Sender<()>>),
    /// Run a single turn without injecting text (operator 'continue').
    RunTurn,
    /// Terminate the agent with an error.
    Bail(String),
    /// Resume autonomous execution (detach the debugger).
    Quit,
}

/// Structured failure returned by [`DebugTarget::attach`] and
/// [`DebugTarget::attach_debugger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachError {
    /// The pause slot's owning execution is gone.
    NoLongerActive,
    /// The execution nonce has changed — target is stale.
    Stale,
    /// Another debugger already holds the lease.
    Busy,
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
    /// A debugger placed a pause request. Crosses turn boundaries. The nonce
    /// is the execution nonce under which the request was made; it lets
    /// [`InteractiveSessionFactory::next_session`] distinguish a between-turn
    /// request from a stale leftover. The acquisition is the lease generation
    /// the attachment acquired, carried so a release can verify its own token.
    DebuggerRequested(CommandInbox, u64, u64),
    /// A message/correction placed a pause request. Does NOT cross turn
    /// boundaries. The nonce is the execution nonce under which the request
    /// was made; the acquisition is the lease generation.
    Requested(CommandInbox, u64, u64),
    /// The agent loop has accepted the pause. Records the execution nonce and
    /// the lease acquisition at the time of acceptance.
    #[allow(dead_code)]
    Accepted { nonce: u64, acquisition: u64 },
}

impl PauseState {
    fn is_pending(&self) -> bool {
        matches!(self, Self::Requested(..) | Self::DebuggerRequested(..))
    }

    fn accept(&mut self) -> Option<CommandInbox> {
        match std::mem::replace(self, Self::Idle) {
            Self::Requested(inbox, nonce, acquisition)
            | Self::DebuggerRequested(inbox, nonce, acquisition) => {
                *self = Self::Accepted { nonce, acquisition };
                Some(inbox)
            }
            previous => {
                *self = previous;
                None
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct PauseSlot {
    state: Mutex<PauseState>,
    requested: Notify,
    /// The end signal for the *current* execution. Replaced when a new
    /// execution is minted; each [`InteractiveSession`] also holds its own
    /// clone so it signals the right one on drop even after a successor has
    /// been minted.
    end_signal: Mutex<EndSignal>,
    /// Identifies one execution (one agent loop invocation). Incremented when
    /// a new session is minted from this slot.
    nonce: AtomicU64,
    /// The run's own id (the gremlin id), used to build execution IDs.
    run_id: Mutex<Option<String>>,
    /// Opaque execution ID published alongside the nonce.
    execution_id: Mutex<Option<String>>,
    /// Increments on every successful attachment so a delayed drop of an old
    /// lease cannot clear a newer lease on the same execution.
    acquisition: AtomicU64,
}

// `AtomicU64` is not `Debug`, so compose the fields we can print by hand.
impl std::fmt::Debug for PauseSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseSlot")
            .field("state", &self.state)
            .field("nonce", &self.nonce)
            .field("execution_id", &self.execution_id)
            .finish_non_exhaustive()
    }
}

impl PauseSlot {
    /// Publish the run id used to build execution IDs.
    pub(crate) fn set_run_id(&self, id: String) {
        *self.run_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    /// Read the run id, if one has been published.
    pub(crate) fn run_id(&self) -> Option<String> {
        self.run_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Publish the opaque execution ID alongside the nonce.
    ///
    /// Callers publish the handle afterwards so nonce and execution ID are
    /// observed as one consistent snapshot.
    pub(crate) fn set_execution_id(&self, id: String) {
        *self.execution_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    /// Read the opaque execution ID, if one has been published.
    pub(crate) fn execution_id(&self) -> Option<String> {
        self.execution_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The end signal of the current execution, cloned for an observer.
    pub(crate) fn current_end_signal(&self) -> EndSignal {
        self.end_signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// End the current execution and install a fresh signal for the next one,
    /// returning the fresh signal. Signals the outgoing one so observers of the
    /// replaced execution wake even before the slot's nonce is bumped.
    fn rotate_end_signal(&self) -> EndSignal {
        let mut guard = self.end_signal.lock().unwrap_or_else(|e| e.into_inner());
        guard.signal();
        *guard = EndSignal::default();
        guard.clone()
    }
}

/// A per-execution end signal: a latch plus a wakeup that an observer can
/// hold independently of the slot.
///
/// `notify_waiters` stores no permit, so an observer that registers after the
/// execution ended would hang. The latch closes that race: [`Self::wait`]
/// registers its wakeup *before* re-checking the flag, so an end that fires at
/// any point — including while the observer is still setting up — is observed.
#[derive(Clone, Debug, Default)]
pub(crate) struct EndSignal {
    ended: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl EndSignal {
    /// Mark the execution ended and wake every registered observer.
    fn signal(&self) {
        self.ended.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Wait until the execution ends, returning immediately if it already has.
    pub(crate) async fn wait(&self) {
        // Register the wakeup before consulting the latch so a `signal` racing
        // this call cannot slip between the check and the await.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_ended() {
            return;
        }
        notified.await;
    }
}

/// The published run handle points to one execution at a time. A debugger
/// snapshots its target *before* validating the active leaf against the store.
#[derive(Clone, Debug, Default)]
pub(crate) struct InteractiveHandle(Arc<Mutex<DebugTarget>>);

impl InteractiveHandle {
    pub(crate) fn target(&self) -> DebugTarget {
        self.0.lock().unwrap().clone()
    }

    /// A handle to the pause slot's end signal, if the target is still alive.
    ///
    /// The returned [`EndSignal`] fires when the captured execution ends —
    /// return, error, cancellation, or replacement by a successor. An
    /// attachment awaiting it stops observing even when no terminal event is
    /// emitted on the bus. The signal latches, so an observer that registers
    /// after the execution already ended still observes it. `None` when the
    /// slot has already been dropped, in which case the caller should treat
    /// the execution as ended.
    pub(crate) fn end_signal(&self) -> Option<EndSignal> {
        let target = self.0.lock().unwrap();
        target.slot.upgrade()?;
        Some(target.end_signal.clone())
    }

    fn publish(&self, slot: &Arc<PauseSlot>) {
        *self.0.lock().unwrap() = DebugTarget {
            slot: Arc::downgrade(slot),
            nonce: slot.nonce.load(Ordering::Relaxed),
            execution_id: slot.execution_id(),
            // Captured under the slot lock, in the same write that publishes
            // the nonce, so the snapshot's signal always belongs to the
            // snapshot's execution — never a successor's.
            end_signal: slot.current_end_signal(),
        };
    }

    #[cfg(test)]
    pub(crate) fn attach(&self) -> Option<DebugAttachment> {
        self.target().attach_debugger().ok()
    }
}

/// Weak by design: neither validation nor a silent debugger keeps a static
/// execution's pending request alive after its agent session goes away.
#[derive(Clone, Debug, Default)]
pub(crate) struct DebugTarget {
    slot: Weak<PauseSlot>,
    nonce: u64,
    /// The opaque execution ID captured when the target was published.
    execution_id: Option<String>,
    /// The end signal for the execution this snapshot describes, captured
    /// atomically with `nonce` so it cannot be confused with a successor's.
    end_signal: EndSignal,
}

impl DebugTarget {
    /// The execution identity captured when the target was published.
    pub(crate) fn nonce(&self) -> u64 {
        self.nonce
    }

    /// The opaque execution ID captured when the target was published.
    pub(crate) fn execution_id(&self) -> Option<&str> {
        self.execution_id.as_deref()
    }

    /// Place a message/correction pause request. The reservation does NOT cross
    /// turn boundaries.
    pub(crate) fn attach(&self) -> Result<DebugAttachment, AttachError> {
        self.attach_impl(false)
    }

    /// Place a debugger pause request. The reservation crosses turn boundaries
    /// so a between-turn debugger's pause-on-entry is honoured.
    pub(crate) fn attach_debugger(&self) -> Result<DebugAttachment, AttachError> {
        self.attach_impl(true)
    }

    /// Shared validation for [`Self::attach`] and [`Self::attach_debugger`].
    /// The two differ only in the `PauseState` variant they reserve.
    fn attach_impl(&self, debugger: bool) -> Result<DebugAttachment, AttachError> {
        let slot = self.slot.upgrade().ok_or(AttachError::NoLongerActive)?;
        if slot.nonce.load(Ordering::Relaxed) != self.nonce {
            return Err(AttachError::Stale);
        }
        let mut state = slot.state.lock().unwrap();
        if !matches!(*state, PauseState::Idle) {
            return Err(AttachError::Busy);
        }
        let acquisition = slot.acquisition.fetch_add(1, Ordering::Relaxed) + 1;
        let (commands, rx) = mpsc::channel(8);
        let (tx, ready) = oneshot::channel();
        let (terminal, ended) = oneshot::channel();
        let inbox = CommandInbox {
            commands: rx,
            ready: Some(tx),
            ended: Some(terminal),
        };
        *state = if debugger {
            PauseState::DebuggerRequested(inbox, self.nonce, acquisition)
        } else {
            PauseState::Requested(inbox, self.nonce, acquisition)
        };
        slot.requested.notify_one();
        Ok(DebugAttachment {
            slot: self.slot.clone(),
            nonce: self.nonce,
            acquisition,
            execution_id: self.execution_id.clone(),
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
    /// The execution nonce this attachment was created under. Used to avoid
    /// clearing a *newer* execution's pause state on drop.
    nonce: u64,
    /// The lease generation this attachment acquired. Drop only clears slot
    /// state when both nonce and acquisition still match.
    acquisition: u64,
    /// The opaque execution ID captured at attach time. Lets the caller
    /// correlate this lease with the execution it targeted.
    execution_id: Option<String>,
    commands: mpsc::Sender<InteractiveCommand>,
    pub(crate) ready: oneshot::Receiver<()>,
    #[allow(dead_code)]
    pub(crate) ended: oneshot::Receiver<DebugOutcome>,
}

impl Drop for DebugAttachment {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.upgrade() {
            // If the nonce has changed, a new execution has started and its
            // pause state is not ours to clear. Likewise a newer lease on the
            // same execution carries a higher acquisition token; an old
            // lease's delayed cleanup must not clear it.
            if slot.nonce.load(Ordering::Relaxed) != self.nonce
                || slot.acquisition.load(Ordering::Relaxed) != self.acquisition
            {
                return;
            }
            let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            let was_pending = state.is_pending();
            *state = PauseState::Idle;
            drop(state);
            // If a request was withdrawn before the agent accepted it,
            // re-notify so any blocked `paused()` waiter can discover the
            // reset and continue without the pause.
            if was_pending {
                slot.requested.notify_one();
            }
        }
    }
}

impl DebugAttachment {
    /// The opaque execution ID captured when this lease was acquired.
    #[allow(dead_code)]
    pub(crate) fn execution_id(&self) -> Option<&str> {
        self.execution_id.as_deref()
    }

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
    /// Every factory keeps a shared pause slot — even pipelines.
    /// This ensures that nonces are globally unique per run and that stale
    /// state (a `Requested` left on the slot by a stage that ignored the
    /// interactive session) is detected and cleared by `next_session`.
    /// Chats additionally allow a debugger request to cross turn boundaries.
    slot: Arc<PauseSlot>,
    chat: bool,
    next_terminal: Option<DebugTerminal>,
    /// A session minted eagerly by [`Self::prepare_next`], handed back by the
    /// next call to [`Self::next_session`].
    prepared: Option<InteractiveSession>,
}

impl InteractiveSessionFactory {
    pub(crate) fn for_pipeline() -> (Self, InteractiveHandle) {
        Self::with_chat(false)
    }

    pub(crate) fn for_chat() -> (Self, InteractiveHandle) {
        Self::with_chat(true)
    }

    fn with_chat(chat: bool) -> (Self, InteractiveHandle) {
        let handle = InteractiveHandle::default();
        let slot: Arc<PauseSlot> = Arc::default();
        handle.publish(&slot);
        (
            Self {
                handle: handle.clone(),
                slot,
                chat,
                next_terminal: None,
                prepared: None,
            },
            handle,
        )
    }

    /// Chat classifies soft vs. hard cancellation after joining the stage.
    pub(crate) fn supervise_next_turn(&mut self) -> DebugTerminal {
        self.next_terminal.insert(DebugTerminal::default()).clone()
    }

    /// Publish the run id used to build execution IDs.
    pub(crate) fn set_run_id(&self, id: String) {
        self.slot.set_run_id(id);
    }

    pub(crate) fn next_session(&mut self) -> InteractiveSession {
        if let Some(mut session) = self.prepared.take() {
            // Always prefer a fresher debugger reservation over whatever
            // terminal the prepared session carries (if any).
            if let Some(terminal) = self.next_terminal.take() {
                session.terminal = Some(terminal);
            }
            return session;
        }
        let mut session = self.mint();
        session.terminal = self.next_terminal.take();
        session
    }

    /// Mint the next execution eagerly and return its published
    /// `execution_id`, stashing the session for [`Self::next_session`].
    ///
    /// Chat turns use this to announce the execution identity at turn start,
    /// before `run_agent` asks for the session.
    #[allow(dead_code)]
    pub(crate) fn prepare_next(&mut self) -> String {
        let session = self.mint();
        let execution_id = session.slot.execution_id().unwrap_or_default();
        self.prepared = Some(session);
        execution_id
    }

    /// Mint a fresh execution: signal the previous one's end, bump the nonce,
    /// publish the execution ID and handle, and sweep stale pause state.
    fn mint(&mut self) -> InteractiveSession {
        let slot = self.slot.clone();
        // End the execution we are replacing (waking its observers) and install
        // a fresh, unlatched end signal for the new one.
        let end_signal = slot.rotate_end_signal();
        // Mint a fresh execution identity *before* sweeping stale state
        // so that the sweep is atomic with respect to the new nonce — a
        // between-turn debugger that has not yet attached sees the new
        // nonce and validates against it, while a request that was
        // already placed under the old nonce is recognised as a
        // between-turn holdover and updated.
        let nonce = slot.nonce.fetch_add(1, Ordering::Relaxed) + 1;
        // Publish the execution ID *before* the single handle publish so the
        // nonce and execution ID are always observed together.
        if let Some(run_id) = slot.run_id() {
            slot.set_execution_id(format!("{run_id}:{nonce}"));
        }
        self.handle.publish(&slot);
        let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            // Stale acceptance (left over from a previous execution whose
            // agent loop is gone) or a message request: reset to Idle so a
            // new debugger can attach.
            PauseState::Accepted {
                nonce: old_nonce, ..
            }
            | PauseState::Requested(_, old_nonce, _)
                if *old_nonce != nonce =>
            {
                *state = PauseState::Idle;
            }
            // Debugger request placed between turns under the previous
            // nonce. Carry it forward to the new execution (chat only).
            PauseState::DebuggerRequested(_, old_nonce, _) if *old_nonce != nonce => {
                if self.chat {
                    if let PauseState::DebuggerRequested(inbox, _, acquisition) =
                        std::mem::replace(&mut *state, PauseState::Idle)
                    {
                        *state = PauseState::DebuggerRequested(inbox, nonce, acquisition);
                    }
                } else {
                    *state = PauseState::Idle;
                }
            }
            _ => {}
        }
        drop(state);
        InteractiveSession {
            slot,
            nonce,
            active: None,
            terminal: None,
            end_signal,
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
    /// This execution's end signal, held so dropping the session signals the
    /// right execution even after a successor has rotated the slot's signal.
    end_signal: EndSignal,
}

/// Ending the execution — return, error or cancellation — wakes every
/// observer waiting on this execution's end signal, even if no successor
/// starts. An `unattached` session carries an isolated signal with no
/// observers, so the drop is a no-op there.
impl Drop for InteractiveSession {
    fn drop(&mut self) {
        self.end_signal.signal();
    }
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

    /// Drop the accepted inbox without firing the `ended` oneshot.
    ///
    /// Used when a correction is delivered through a retained debugger lease:
    /// the socket must stay connected and the lease must remain live for the
    /// next operator command, so we must not emit a terminal outcome.
    pub(crate) fn finish_attachment_without_outcome(&mut self) {
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
    async fn end_signal_fires_when_the_execution_drops() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let session = sessions.next_session();

        // An observer captures the end signal and registers a waiter before
        // the execution ends.
        let signal = handle.end_signal().expect("slot is live");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            signal.wait().await;
            let _ = tx.send(());
        });
        // Let the spawned waiter reach `wait()` (register) first.
        tokio::task::yield_now().await;

        // Dropping the execution wakes the observer even though no terminal
        // bus event is ever emitted.
        drop(session);
        tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("end signal must fire when the execution ends")
            .expect("waiter task ended without signalling");
        waiter.await.unwrap();
    }

    #[tokio::test]
    async fn end_signal_latches_so_a_late_waiter_still_observes_it() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let session = sessions.next_session();
        let signal = handle.end_signal().expect("slot is live");

        // The execution ends *before* the observer registers: this is the
        // window `notify_waiters` alone would drop. The latch must still make
        // the wait complete.
        drop(session);
        tokio::time::timeout(std::time::Duration::from_secs(1), signal.wait())
            .await
            .expect("an already-ended execution must complete wait immediately");
    }

    #[tokio::test]
    async fn end_signal_fires_when_a_successor_execution_starts() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let signal = handle.end_signal().expect("slot is live");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            signal.wait().await;
            let _ = tx.send(());
        });
        tokio::task::yield_now().await;

        // Minting the next execution replaces this one; the observer of the
        // replaced execution is woken.
        let _second = sessions.next_session();
        tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("a successor execution must wake the previous observer")
            .expect("waiter task ended without signalling");
        waiter.await.unwrap();
        drop(first);
    }

    #[tokio::test]
    async fn end_signal_rotates_per_execution() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let first_signal = handle.end_signal().expect("slot is live");
        let second = sessions.next_session();
        let second_signal = handle.end_signal().expect("slot is live");

        // Ending the first execution must not end the second: the signals are
        // distinct and only the outgoing one is latched.
        drop(first);

        // Use a oneshot wired to the second signal to detect an erroneous
        // early completion. The oneshot fires only if second_signal completes
        // before we explicitly drop second — which must not happen.
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let second_signal_clone = second_signal.clone();
        tokio::spawn(async move {
            second_signal_clone.wait().await;
            let _ = tx.send(());
        });
        // Not yielded: the spawned task races us. If the signal fires
        // spuriously, rx will produce a value.
        assert!(
            rx.try_recv().is_err(),
            "the new execution's signal must stay unlatched"
        );
        // The outgoing execution's signal is latched.
        tokio::time::timeout(std::time::Duration::from_secs(1), first_signal.wait())
            .await
            .expect("the replaced execution's signal must be latched");
        drop(second);
    }

    #[tokio::test]
    async fn end_signal_is_absent_once_the_slot_is_gone() {
        let (sessions, handle) = InteractiveSessionFactory::for_chat();
        drop(sessions);
        assert!(handle.end_signal().is_none());
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
