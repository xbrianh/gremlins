//! The run handle publishes pause requests; only the loop that accepts a
//! request receives that attachment's commands. Stream events stay on the bus.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

static NEXT_LEASE_ID: AtomicU64 = AtomicU64::new(1);

use crate::clients::cancel::{CancelListener, CancelToken};
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
    acquisition: u64,
    retained: bool,
    handed_off: bool,
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

    fn transfer_to(&self, next: &DebugTerminal) {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = current.take() {
            *next.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
        }
    }
}

#[derive(Debug, Default)]
enum PauseState {
    #[default]
    Idle,
    /// A debugger placed a pause request. Crosses turn boundaries only when
    /// stored on the registration reservation slot. The acquisition is the lease generation
    /// the attachment acquired, carried so a release can verify its own token.
    DebuggerRequested(CommandInbox, u64, u64, Arc<Mutex<Weak<PauseSlot>>>),
    /// A message/correction placed a pause request. Does NOT cross turn
    /// boundaries. The nonce is the execution nonce under which the request
    /// was made; the acquisition is the lease generation.
    Requested(CommandInbox, u64, u64, Arc<Mutex<Weak<PauseSlot>>>),
    /// The agent loop has accepted the pause. Records the execution nonce and
    /// the lease acquisition at the time of acceptance.
    #[allow(dead_code)]
    Accepted { nonce: u64, acquisition: u64 },
}

impl PauseState {
    fn is_pending(&self) -> bool {
        matches!(self, Self::Requested(..) | Self::DebuggerRequested(..))
    }

    fn accept(&mut self, nonce: u64) -> Option<CommandInbox> {
        match std::mem::replace(self, Self::Idle) {
            Self::DebuggerRequested(mut inbox, request_nonce, acquisition, _owner)
                if request_nonce == nonce =>
            {
                inbox.handed_off = true;
                *self = Self::Accepted {
                    nonce: request_nonce,
                    acquisition,
                };
                Some(inbox)
            }
            Self::Requested(inbox, request_nonce, acquisition, _owner)
                if request_nonce == nonce =>
            {
                *self = Self::Accepted {
                    nonce: request_nonce,
                    acquisition,
                };
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
    /// Only chat slots may reserve debugger commands between executions.
    chat: bool,
    requested: Arc<Notify>,
    /// End signal for this execution, independently held by snapshots.
    end_signal: Mutex<EndSignal>,
    /// Identifies this execution (one agent loop invocation).
    nonce: AtomicU64,
    /// Opaque execution ID published alongside the nonce.
    execution_id: Mutex<Option<String>>,
    /// Actual backend capability for this execution.
    commands_supported: AtomicBool,
    /// Closed when the factory that owns a chat registration is dropped.
    registration_closed: AtomicBool,
}

// Keep diagnostics focused on execution identity and ownership state.
impl std::fmt::Debug for PauseSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseSlot")
            .field("state", &self.state)
            .field("nonce", &self.nonce)
            .field("execution_id", &self.execution_id)
            .finish_non_exhaustive()
    }
}

impl CancelListener for PauseSlot {
    fn on_cancel(&self) {
        self.end_execution(
            self.nonce.load(Ordering::Acquire),
            &self.current_end_signal(),
        );
    }
}

impl PauseSlot {
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

    /// End and revoke an execution under the same lock used by acceptance.
    /// This makes acceptance and revocation a single ordered protocol: either
    /// acceptance wins before the end, or the ended inbox can never be accepted.
    fn end_execution(&self, nonce: u64, signal: &EndSignal) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        signal.signal();
        let matches = match &*state {
            PauseState::Requested(_, n, _, _)
            | PauseState::DebuggerRequested(_, n, _, _)
            | PauseState::Accepted { nonce: n, .. } => *n == nonce,
            PauseState::Idle => false,
        };
        if matches {
            *state = PauseState::Idle;
            drop(state);
            self.requested.notify_waiters();
        }
    }

    /// Close a chat registration and revoke any reservation that cannot be
    /// handed to a future execution.
    fn close_registration(&self) {
        self.registration_closed.store(true, Ordering::Release);
        self.current_end_signal().signal();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_pending() {
            *state = PauseState::Idle;
            drop(state);
            self.requested.notify_waiters();
        }
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

    pub(crate) fn is_ended(&self) -> bool {
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
pub(crate) struct InteractiveHandle(Arc<Mutex<DebugTarget>>, Arc<PauseSlot>);

impl InteractiveHandle {
    pub(crate) fn target(&self) -> DebugTarget {
        self.0.lock().unwrap().clone()
    }

    /// Capture setup intent with the target, before its execution can expire.
    pub(crate) fn attachment_target(&self) -> (DebugTarget, bool) {
        let target = self.0.lock().unwrap();
        let between_turn = target.registration_only || target.end_signal.is_ended();
        (target.clone(), between_turn)
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
    #[cfg(test)]
    pub(crate) fn end_signal(&self) -> Option<EndSignal> {
        let target = self.0.lock().unwrap();
        // A live registration slot before first publication is an observer
        // lifetime, while a dead slot means the registration itself is gone.
        target.slot.upgrade()?;
        Some(target.end_signal.clone())
    }

    pub(crate) fn attach_next_turn_debugger(
        &self,
        execution_id: Option<String>,
    ) -> Result<DebugAttachment, AttachError> {
        let slot = self.1.clone();
        let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.chat || slot.registration_closed.load(Ordering::Acquire) {
            return Err(AttachError::Busy);
        }
        if !matches!(*state, PauseState::Idle) {
            return Err(AttachError::Busy);
        }
        let target = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let nonce = target.nonce;
        let observed_end = target.end_signal.clone();
        // The reservation-state lock serializes this check with `mint`, which
        // publishes each new turn while holding the same lock. A live turn
        // means the request is not between turns and must not reserve its
        // successor.
        if !target.readonly && target.slot.upgrade().is_some() && !observed_end.is_ended() {
            return Err(AttachError::Busy);
        }
        drop(target);
        let acquisition = NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed);
        let owner = Arc::new(Mutex::new(Arc::downgrade(&slot)));
        let (commands, rx) = mpsc::channel(8);
        let (tx, ready) = oneshot::channel();
        let (terminal, ended) = oneshot::channel();
        *state = PauseState::DebuggerRequested(
            CommandInbox {
                acquisition,
                retained: true,
                handed_off: false,
                commands: rx,
                ready: Some(tx),
                ended: Some(terminal),
            },
            nonce,
            acquisition,
            owner.clone(),
        );
        Ok(DebugAttachment {
            owner,
            reservation_slot: Some(Arc::downgrade(&slot)),
            nonce,
            acquisition,
            debugger: true,
            execution_id,
            commands,
            ready,
            ended,
        })
    }

    fn publish(&self, slot: &Arc<PauseSlot>) {
        *self.0.lock().unwrap() = DebugTarget {
            slot: Arc::downgrade(slot),
            nonce: slot.nonce.load(Ordering::Acquire),
            execution_id: slot.execution_id(),
            // Captured under the slot lock, in the same write that publishes
            // the nonce, so the snapshot's signal always belongs to the
            // snapshot's execution — never a successor's.
            end_signal: slot.current_end_signal(),
            readonly: false,
            commands_supported: slot.commands_supported.load(Ordering::Acquire),
            registration_only: false,
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
    /// A passive target may be observed but cannot reserve command capabilities.
    readonly: bool,
    /// Backend capability captured by this immutable target snapshot.
    commands_supported: bool,
    end_signal: EndSignal,
    /// True only for the registration observation published before first mint.
    registration_only: bool,
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

    #[allow(dead_code)]
    pub(crate) fn end_signal(&self) -> EndSignal {
        self.end_signal.clone()
    }

    /// Whether this snapshot permits interactive control.
    pub(crate) fn accepts_commands(&self) -> bool {
        !self.readonly && self.commands_supported
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

    /// Shared validation for message/correction and debugger pause requests.
    /// The two differ only in the `PauseState` variant they reserve.
    fn attach_impl(&self, debugger: bool) -> Result<DebugAttachment, AttachError> {
        if self.readonly {
            return Err(AttachError::Busy);
        }
        let slot = self.slot.upgrade().ok_or(AttachError::NoLongerActive)?;
        let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if slot.registration_closed.load(Ordering::Acquire) {
            return Err(AttachError::NoLongerActive);
        }
        if slot.nonce.load(Ordering::Acquire) != self.nonce {
            return Err(AttachError::Stale);
        }
        if slot.current_end_signal().is_ended() {
            return Err(AttachError::Stale);
        }
        if self.execution_id != slot.execution_id() {
            return Err(AttachError::Stale);
        }
        if !self.commands_supported || !slot.commands_supported.load(Ordering::Acquire) {
            return Err(AttachError::Busy);
        }
        if !matches!(*state, PauseState::Idle) {
            return Err(AttachError::Busy);
        }
        let acquisition = NEXT_LEASE_ID.fetch_add(1, Ordering::Relaxed);
        let owner = Arc::new(Mutex::new(Arc::downgrade(&slot)));
        let (commands, rx) = mpsc::channel(8);
        let (tx, ready) = oneshot::channel();
        let (terminal, ended) = oneshot::channel();
        let inbox = CommandInbox {
            acquisition,
            retained: debugger,
            handed_off: false,
            commands: rx,
            ready: Some(tx),
            ended: Some(terminal),
        };
        *state = if debugger {
            PauseState::DebuggerRequested(inbox, self.nonce, acquisition, owner.clone())
        } else {
            PauseState::Requested(inbox, self.nonce, acquisition, owner.clone())
        };
        slot.requested.notify_one();
        Ok(DebugAttachment {
            owner,
            reservation_slot: None,
            nonce: self.nonce,
            acquisition,
            debugger,
            execution_id: self.execution_id.clone(),
            commands,
            ready,
            ended,
        })
    }

    /// Return a target snapshot that is observation-only.
    #[allow(dead_code)]
    pub(crate) fn read_only(&self) -> Self {
        let mut target = self.clone();
        target.readonly = true;
        target
    }
}

/// Withdrawing a pending pause and accepting it are mutually exclusive.
/// If already accepted, dropping the sole sender wakes just that receiver.
#[derive(Debug)]
pub(crate) struct DebugAttachment {
    owner: Arc<Mutex<Weak<PauseSlot>>>,
    /// Stable registration slot used to serialize handoff and withdrawal.
    reservation_slot: Option<Weak<PauseSlot>>,
    /// The execution nonce this attachment was created under.
    nonce: u64,
    /// The lease generation this attachment acquired. Drop only clears slot
    /// state when both nonce and acquisition still match.
    acquisition: u64,
    /// Debugger reservations may be handed across chat-turn boundaries.
    debugger: bool,
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
        // Between-turn handoff and withdrawal both take reservation -> owner
        // -> live slot. Direct execution leases only take owner -> live slot.
        let reservation_slot = self.reservation_slot.as_ref().and_then(Weak::upgrade);
        let mut reservation = reservation_slot
            .as_ref()
            .map(|slot| slot.state.lock().unwrap_or_else(|e| e.into_inner()));
        let pending_here = self.reservation_slot.is_some() && reservation.as_ref().is_some_and(|state| {
            matches!(
                &**state,
                PauseState::DebuggerRequested(_, _, acquisition, _) if *acquisition == self.acquisition
            )
        });
        let reservation_empty = reservation
            .as_ref()
            .is_some_and(|state| matches!(**state, PauseState::Idle));
        if pending_here {
            if let Some(state) = reservation.as_mut() {
                **state = PauseState::Idle;
            }
            drop(reservation);
            if let Some(slot) = reservation_slot.as_ref() {
                slot.requested.notify_waiters();
            }
            return;
        }
        // A consumed reservation either moved to the live slot or was rejected.
        // Release registration state before inspecting the owner route.
        let handoff_consumed = self.reservation_slot.is_some() && reservation_empty;
        drop(reservation);
        if handoff_consumed {
            if let Some(slot) = reservation_slot.as_ref() {
                slot.requested.notify_waiters();
            }
            if self.owner_is_invalid() {
                return;
            }
        }
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = owner.upgrade() {
            // If the nonce has changed, a new execution has started and its
            // pause state is not ours to clear. Likewise a newer lease on the
            // same execution carries a higher acquisition token; an old
            // lease's delayed cleanup must not clear it.
            let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            let owns_state = match &*state {
                PauseState::Requested(_, nonce, acquisition, _)
                | PauseState::DebuggerRequested(_, nonce, acquisition, _)
                | PauseState::Accepted { nonce, acquisition } => {
                    *acquisition == self.acquisition
                        && (*nonce == self.nonce || (self.debugger && *nonce > self.nonce))
                }
                PauseState::Idle => false,
            };
            if !owns_state {
                return;
            }
            *state = PauseState::Idle;
            drop(state);
            slot.requested.notify_one();
        }
        drop(owner);
    }
}

impl DebugAttachment {
    /// Snapshot only the execution that accepted this acquisition, including
    /// a between-turn reservation's explicit owner handoff.
    pub(crate) fn bound_target(&self) -> Result<DebugTarget, AttachError> {
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        let slot = owner.upgrade().ok_or(AttachError::NoLongerActive)?;
        let state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        let nonce = match &*state {
            PauseState::Accepted { nonce, acquisition } if *acquisition == self.acquisition => {
                *nonce
            }
            _ => return Err(AttachError::Stale),
        };
        let end_signal = slot.current_end_signal();
        if slot.nonce.load(Ordering::Acquire) != nonce || end_signal.is_ended() {
            return Err(AttachError::Stale);
        }
        Ok(DebugTarget {
            slot: Arc::downgrade(&slot),
            nonce,
            execution_id: slot.execution_id(),
            end_signal,
            readonly: false,
            commands_supported: slot.commands_supported.load(Ordering::Acquire),
            registration_only: false,
        })
    }

    fn owner_is_invalid(&self) -> bool {
        self.owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
            .is_none()
    }

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
    /// Current live execution slot.
    current_slot: Option<Arc<PauseSlot>>,
    chat: bool,
    next_terminal: Option<DebugTerminal>,
    /// Stable chat-registration inbox for between-turn debugger reservations.
    reservation_slot: Arc<PauseSlot>,
    /// Registration-local execution nonce, independent from the opaque ID.
    next_nonce: u64,
    /// A session minted eagerly by [`Self::prepare_next_with_commands`].
    prepared: Option<InteractiveSession>,
}

impl Drop for InteractiveSessionFactory {
    fn drop(&mut self) {
        self.reservation_slot.close_registration();
    }
}

impl InteractiveSessionFactory {
    pub(crate) fn for_pipeline() -> (Self, InteractiveHandle) {
        Self::with_chat(false)
    }

    pub(crate) fn for_chat() -> (Self, InteractiveHandle) {
        Self::with_chat(true)
    }

    fn with_chat(chat: bool) -> (Self, InteractiveHandle) {
        let registration_slot = Arc::new(PauseSlot {
            chat,
            ..PauseSlot::default()
        });
        // Before the first execution is published, passive observers still
        // need a lifetime to wait on. Keep this registration-only target
        // observation-only; the first mint ends its signal and publishes the
        // actual execution target.
        let registration_signal = registration_slot.current_end_signal();
        let handle = InteractiveHandle(
            Arc::new(Mutex::new(DebugTarget {
                slot: Arc::downgrade(&registration_slot),
                readonly: true,
                end_signal: registration_signal,
                registration_only: true,
                ..DebugTarget::default()
            })),
            registration_slot.clone(),
        );
        (
            Self {
                handle: handle.clone(),
                current_slot: None,
                chat,
                next_terminal: None,
                reservation_slot: registration_slot,
                next_nonce: 1,
                prepared: None,
            },
            handle,
        )
    }

    /// Chat classifies soft vs. hard cancellation after joining the stage.
    pub(crate) fn supervise_next_turn(&mut self) -> DebugTerminal {
        self.next_terminal.insert(DebugTerminal::default()).clone()
    }

    /// Kept for the execution factory interface; run IDs are deliberately not
    /// incorporated into public execution identities.
    #[allow(dead_code)]
    pub(crate) fn set_run_id(&self, _id: String) {}

    #[cfg(test)]
    pub(crate) fn next_session(&mut self) -> InteractiveSession {
        self.next_session_with_commands(true, None).unwrap()
    }

    /// Mint a session with its backend capability fixed before publication.
    pub(crate) fn next_session_with_commands(
        &mut self,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<InteractiveSession, &'static str> {
        if let Some(mut session) = self.prepared.take() {
            // The prepared session's identity has already been published.
            // Never replace it if backend selection disagrees; fail the caller's
            // turn while dropping this execution revokes the announced identity.
            if session.accepts_commands != accepts_commands {
                drop(session);
                return Err("prepared interactive session capability disagrees with backend");
            }
            if let Some(terminal) = self.next_terminal.take() {
                if let Some(previous) = &session.terminal {
                    previous.transfer_to(&terminal);
                }
                session.terminal = Some(terminal);
            }
            return Ok(session);
        }
        Ok(self.mint(accepts_commands, cancel))
    }

    /// Drop only a prepared execution. An absent preparation is a no-op.
    pub(crate) fn discard_prepared(&mut self) {
        drop(self.prepared.take());
    }

    /// Mint the next execution eagerly and return its published
    /// `execution_id`, stashing the session for its next-session call.
    ///
    /// Chat turns use this to announce the execution identity at turn start,
    /// before `run_agent` asks for the session.
    #[allow(dead_code)]
    pub(crate) fn prepare_next(&mut self) -> String {
        self.prepare_next_with_commands(true, None)
    }

    /// Prepare a session with its command capability fixed before publishing.
    pub(crate) fn prepare_next_with_commands(
        &mut self,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> String {
        let mut session = self.mint(accepts_commands, cancel);
        if let Some(terminal) = self.next_terminal.take() {
            if let Some(previous) = &session.terminal {
                previous.transfer_to(&terminal);
            }
            session.terminal = Some(terminal);
        }
        let execution_id = session.slot.execution_id().unwrap_or_default();
        self.prepared = Some(session);
        execution_id
    }

    /// Prepare and capture an execution from this factory, never a registry lookup.
    pub(crate) fn prepare_target_with_commands(
        &mut self,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> DebugTarget {
        self.prepare_next_with_commands(accepts_commands, cancel);
        self.handle.target()
    }

    /// Mint a fresh execution: signal the previous one's end, bump the nonce,
    /// publish the execution ID and handle, and sweep stale pause state.
    fn mint(&mut self, accepts_commands: bool, cancel: Option<&CancelToken>) -> InteractiveSession {
        let outgoing_terminal = self.prepared.as_ref().and_then(|s| s.terminal.clone());
        let previous = self.current_slot.take();
        let mut reservation = self
            .reservation_slot
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if previous.is_none() {
            // The registration-only observation lifetime ends exactly when
            // the first execution is published.
            self.handle
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .end_signal
                .signal();
        }
        let slot: Arc<PauseSlot> = Arc::new(PauseSlot {
            chat: self.chat,
            commands_supported: AtomicBool::new(accepts_commands),
            ..PauseSlot::default()
        });
        let mut inherited_terminal = None;
        if let Some(previous) = &previous {
            let mut previous_state = previous.state.lock().unwrap_or_else(|e| e.into_inner());
            previous.current_end_signal().signal();
            previous.requested.notify_waiters();
            // Every unaccepted request belongs to this execution. Revoke it;
            // only the independent between-turn reservation can be handed on.
            // Replacement is the same revocation transition as normal
            // execution end: accepted ownership is revoked as well as pending
            // requests, under the state lock shared by `next_command`.
            let previous_nonce = previous.nonce.load(Ordering::Acquire);
            let matches = match &*previous_state {
                PauseState::Requested(_, n, _, _)
                | PauseState::DebuggerRequested(_, n, _, _)
                | PauseState::Accepted { nonce: n, .. } => *n == previous_nonce,
                PauseState::Idle => false,
            };
            if matches {
                *previous_state = PauseState::Idle;
            }
        }
        {
            let pending = std::mem::replace(&mut *reservation, PauseState::Idle);
            let pending = match pending {
                PauseState::DebuggerRequested(inbox, nonce, acquisition, owner) => {
                    Some((inbox, nonce, acquisition, owner))
                }
                other => {
                    *reservation = other;
                    None
                }
            };
            if let Some((mut inbox, _, acquisition, owner)) = pending {
                if accepts_commands {
                    // Lock order for lease routing is reservation state -> owner ->
                    // live state. Attachment drop uses the same order, so it either
                    // withdraws before handoff or follows the accepted acquisition.
                    let mut owner_slot = owner.lock().unwrap_or_else(|e| e.into_inner());
                    inbox.handed_off = true;
                    *slot.state.lock().unwrap_or_else(|e| e.into_inner()) =
                        PauseState::DebuggerRequested(
                            inbox,
                            self.next_nonce,
                            acquisition,
                            owner.clone(),
                        );
                    *owner_slot = Arc::downgrade(&slot);
                    // The attachment now routes directly to the live slot;
                    // withdrawal no longer needs the reservation slot.
                    inherited_terminal = self.next_terminal.take().or(outgoing_terminal);
                } else {
                    // The request is rejected, so make the owner route inert.
                    // DebugAttachment::drop sees the empty reservation and
                    // avoids attempting to lock that slot again.
                    *owner.lock().unwrap_or_else(|e| e.into_inner()) = Weak::new();
                    drop(inbox);
                    if let Some(tx) = self.next_terminal.take().and_then(|terminal| {
                        terminal.0.lock().unwrap_or_else(|e| e.into_inner()).take()
                    }) {
                        let _ = tx.send(DebugOutcome::Ended {
                            reason: "turn cannot accept commands".into(),
                        });
                    }
                }
            }
        }
        let nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);
        slot.nonce.store(nonce, Ordering::Release);
        slot.set_execution_id(format!("exec-{}", uuid::Uuid::new_v4()));
        let end_signal = slot.current_end_signal();
        if let Some(cancel) = cancel {
            cancel.listen(slot.clone());
        }
        self.handle.publish(&slot);
        drop(reservation);
        self.current_slot = Some(slot.clone());
        if let Some(terminal) = self.next_terminal.take() {
            inherited_terminal = Some(terminal);
        }
        InteractiveSession {
            slot,
            nonce,
            active: None,
            terminal: inherited_terminal,
            end_signal,
            accepts_commands,
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
    accepts_commands: bool,
}

/// Ending the execution — return, error or cancellation — wakes every
/// observer waiting on this execution's end signal, even if no successor
/// starts. An `unattached` session carries an isolated signal with no
/// observers, so the drop is a no-op there.
impl Drop for InteractiveSession {
    fn drop(&mut self) {
        self.slot.end_execution(self.nonce, &self.end_signal);
    }
}

impl InteractiveSession {
    pub fn unattached() -> Self {
        Self::default()
    }

    /// Snapshot this execution directly, never the factory's latest registration.
    pub(crate) fn snapshot(&self) -> DebugTarget {
        DebugTarget {
            slot: Arc::downgrade(&self.slot),
            nonce: self.nonce,
            execution_id: self.slot.execution_id(),
            readonly: false,
            commands_supported: self.accepts_commands,
            end_signal: self.end_signal.clone(),
            registration_only: false,
        }
    }

    pub(crate) fn accepts_commands(&self) -> bool {
        self.accepts_commands
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
        if self.end_signal.is_ended() {
            return false;
        }
        let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
        // Keep a retained debugger's local inbox until the agent has restored
        // its prompt and announced withdrawal. A successor must not erase it.
        if self.active.as_ref().is_some_and(|inbox| inbox.retained) {
            return false;
        }
        let Some(mut inbox) = state.accept(self.nonce) else {
            return false;
        };
        drop(state);
        if let Some(terminal) = &self.terminal {
            *terminal.0.lock().unwrap_or_else(|e| e.into_inner()) = inbox.ended.take();
        }
        inbox.handed_off = true;
        self.active = Some(inbox);
        true
    }

    pub(crate) async fn paused(&mut self) -> bool {
        loop {
            if self.has_revoked_debugger() || self.accept_pause() {
                return true;
            }
            let notify = self.slot.requested.clone();
            let requested = notify.notified();
            tokio::pin!(requested);
            requested.as_mut().enable();
            if self.has_revoked_debugger() || self.accept_pause() {
                return true;
            }
            tokio::select! {
                biased;
                _ = self.end_signal.wait() => return false,
                _ = requested => {},
            }
            if self.end_signal.is_ended() {
                return false;
            }
        }
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn execution_ended(&self) -> bool {
        self.end_signal.is_ended()
    }

    /// Drop a stale local inbox after its lease was revoked. This deliberately
    /// does not mutate slot state or the supervisor-owned terminal sender.
    pub(crate) fn discard_revoked_inbox(&mut self) {
        if !self.has_accepted_acquisition() {
            self.active = None;
        }
    }

    /// Continue autonomously after a debugger lease is withdrawn. Retained
    /// debugger leases are not removed by delivery of an ordinary message.
    pub(crate) fn detach_revoked_debugger(&mut self) -> bool {
        let revoked_debugger = self.has_revoked_debugger();
        if revoked_debugger {
            self.release_accepted_lease();
        }
        revoked_debugger
    }

    pub(crate) fn has_revoked_debugger(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|inbox| inbox.retained && !self.has_accepted_acquisition())
    }

    pub(crate) fn has_accepted_acquisition(&self) -> bool {
        let Some(inbox) = &self.active else {
            return false;
        };
        let state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
        matches!(*state, PauseState::Accepted { nonce, acquisition }
            if nonce == self.nonce && acquisition == inbox.acquisition)
    }

    pub(crate) fn finish_attachment(&mut self, outcome: DebugOutcome) -> bool {
        let Some(inbox) = self.active.as_mut() else {
            return false;
        };
        let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(*state, PauseState::Accepted { nonce, acquisition }
            if nonce == self.nonce && acquisition == inbox.acquisition)
        {
            drop(state);
            self.active = None;
            return false;
        }
        if let Some(terminal) = &self.terminal {
            terminal.finish(outcome);
        } else if let Some(tx) = inbox.ended.take() {
            let _ = tx.send(outcome);
        }
        *state = PauseState::Idle;
        drop(state);
        self.active = None;
        self.slot.requested.notify_one();
        true
    }

    /// Drop the accepted inbox without firing the `ended` oneshot.
    ///
    /// Used when a correction is delivered through a retained debugger lease:
    /// the socket must stay connected and the lease must remain live for the
    /// next operator command, so we must not emit a terminal outcome.
    pub(crate) fn finish_message_delivery(&mut self) {
        let retained_and_accepted = self
            .active
            .as_ref()
            .is_some_and(|inbox| inbox.retained && self.has_accepted_acquisition());
        if !retained_and_accepted {
            self.release_accepted_lease();
        }
    }

    /// Release only the accepted lease owned by this execution's active inbox.
    fn release_accepted_lease(&mut self) {
        let Some(inbox) = self.active.take() else {
            return;
        };
        let acquisition = inbox.acquisition;
        // The inbox is accepted only after `PauseState::accept` has consumed
        // its request. Match the exact lease, not merely this execution.
        let mut state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, PauseState::Accepted { acquisition: current, .. } if current == acquisition)
        {
            *state = PauseState::Idle;
            drop(state);
            self.slot.requested.notify_one();
        } else {
            drop(state);
        }
        drop(inbox);
    }

    /// Called after Ready has been emitted, including when re-entering after
    /// a single step. Only the first Ready completes the socket handshake.
    pub(crate) async fn next_command(&mut self) -> Option<InteractiveCommand> {
        let inbox = self.active.as_mut()?;
        // Releasing an accepted attachment revokes its acquisition even when
        // this execution remains live. Check under the state lock so stale
        // inboxes neither announce readiness nor consume buffered commands.
        {
            let state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(*state, PauseState::Accepted { nonce, acquisition }
                if nonce == self.nonce && acquisition == inbox.acquisition)
            {
                return None;
            }
            if let Some(ready) = inbox.ready.take() {
                let _ = ready.send(());
            }
        }
        tokio::select! {
            biased;
            _ = self.end_signal.wait() => None,
            command = inbox.commands.recv() => {
                let state = self.slot.state.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(*state, PauseState::Accepted { nonce, acquisition }
                    if nonce == self.nonce && acquisition == inbox.acquisition)
                {
                    command
                } else {
                    None
                }
            },
        }
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
    async fn cancellation_revokes_execution_while_backend_is_held() {
        for prepared in [false, true] {
            let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
            let cancel = CancelToken::new();
            if prepared {
                sessions.prepare_next_with_commands(true, Some(&cancel));
            }
            let session = sessions
                .next_session_with_commands(true, Some(&cancel))
                .unwrap();
            let target = handle.target();
            let mut pending = target.attach().unwrap();
            let (release, hold) = oneshot::channel();
            // Poll the backend through entry, but don't let it drop the session.
            let mut backend = Box::pin(async move {
                hold.await.unwrap();
                drop(session);
            });
            assert!(backend.as_mut().now_or_never().is_none());
            cancel.cancel();
            assert!(target.end_signal().is_ended());
            assert!((&mut pending.ready).now_or_never().unwrap().is_err());
            assert!(matches!(target.attach(), Err(AttachError::Stale)));
            release.send(()).unwrap();
            backend.await;
        }
    }

    #[test]
    fn cancellation_during_preparation_survives_backend_handoff() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let cancel = CancelToken::new();
        sessions.prepare_next_with_commands(true, Some(&cancel));
        let target = handle.target();
        let mut pending = target.attach().unwrap();
        cancel.cancel();
        assert!(target.end_signal().is_ended());
        assert!((&mut pending.ready).now_or_never().unwrap().is_err());
        let mut session = sessions
            .next_session_with_commands(true, Some(&cancel))
            .unwrap();
        assert!(!session.accept_pause());
        assert!(matches!(handle.target().attach(), Err(AttachError::Stale)));
    }

    #[tokio::test]
    async fn late_cancel_keeps_successor_and_supervised_outcome() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let cancel = CancelToken::new();
        let terminal = sessions.supervise_next_turn();
        let mut first = sessions
            .next_session_with_commands(true, Some(&cancel))
            .unwrap();
        let mut debugger = handle.target().attach_debugger().unwrap();
        assert!(first.accept_pause());
        assert!(first.next_command().now_or_never().is_none());
        (&mut debugger.ready).now_or_never().unwrap().unwrap();
        let mut second = sessions.next_session();
        let successor = handle.target().attach().unwrap();
        cancel.cancel();
        assert!(second.accept_pause());
        assert!(!handle.target().end_signal().is_ended());
        assert!((&mut debugger.ended).now_or_never().is_none());
        terminal.end("interrupted");
        assert!(
            matches!((&mut debugger.ended).await, Ok(DebugOutcome::Ended {reason}) if reason == "interrupted")
        );
        drop(successor);
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

        // Poll the observer to Pending so this covers an already-registered
        // wakeup (the separate latch test covers a late waiter).
        let signal = handle.end_signal().expect("slot is live");
        let waiting = signal.wait();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());

        drop(session);
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
            .await
            .expect("end signal must fire when the execution ends");
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
    async fn command_wait_ends_when_execution_is_replaced() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut first = sessions.next_session();
        let mut owner = handle.target().attach().unwrap();
        assert!(first.accept_pause());
        let waiting = first.next_command();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        (&mut owner.ready).await.unwrap();
        owner.try_send(InteractiveCommand::RunTurn).unwrap();
        let _successor = sessions.next_session();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
                .await
                .expect("replacement wakes the command wait")
                .is_none()
        );
    }

    #[tokio::test]
    async fn end_signal_fires_when_a_successor_execution_starts() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let signal = handle.end_signal().expect("slot is live");
        let waiting = signal.wait();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());

        // Minting the next execution replaces this one and wakes the already
        // registered observer.
        let _second = sessions.next_session();
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
            .await
            .expect("a successor execution must wake the previous observer");
        drop(first);
    }

    #[tokio::test]
    async fn end_signal_rotates_per_execution() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let first_signal = handle.end_signal().expect("slot is live");
        let second = sessions.next_session();
        let second_signal = handle.end_signal().expect("slot is live");

        // Drop the old session, then directly poll the successor's waiter
        // before dropping it. This fails if the old drop latched both signals.
        drop(first);
        let second_wait = second_signal.wait();
        tokio::pin!(second_wait);
        assert!(futures::poll!(&mut second_wait).is_pending());
        assert!(first_signal.is_ended());
        assert!(!second_signal.is_ended(), "successor signal ended early");

        drop(second);
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut second_wait)
            .await
            .expect("the successor signal must latch on its own drop");
    }

    #[tokio::test]
    async fn pre_mint_observer_ends_when_first_execution_is_published() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let signal = handle.end_signal().expect("registration is alive");
        assert!(matches!(handle.target().attach(), Err(AttachError::Busy)));
        let waiting = signal.wait();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());

        let _session = sessions.next_session();
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
            .await
            .expect("first publication ends the registration observation");
        assert!(!handle.end_signal().unwrap().is_ended());
    }

    #[tokio::test]
    async fn dropping_pre_mint_factory_ends_observers_and_rejects_reservations() {
        let (factory, handle) = InteractiveSessionFactory::for_chat();
        let signal = handle.end_signal().expect("registration is alive");
        let mut reservation = handle.attach_next_turn_debugger(None).unwrap();
        drop(factory);

        tokio::time::timeout(std::time::Duration::from_secs(1), signal.wait())
            .await
            .expect("factory teardown must end registration observers");
        assert!((&mut reservation.ready).now_or_never().unwrap().is_err());
        assert!(matches!(
            handle.attach_next_turn_debugger(None),
            Err(AttachError::Busy)
        ));
        assert!(handle.end_signal().unwrap().is_ended());
    }

    #[tokio::test]
    async fn end_signal_is_absent_once_the_slot_is_gone() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let execution = sessions.next_session();
        drop(sessions);
        drop(execution);
        assert!(handle.end_signal().is_none());
    }

    #[tokio::test]
    async fn ending_execution_revokes_unaccepted_request_immediately() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_pipeline();
        let execution = sessions.next_session();
        let target = handle.target();
        let mut request = target.attach().unwrap();
        let signal = target.end_signal();
        let ended = signal.wait();
        tokio::pin!(ended);

        drop(execution);
        assert!(ended.now_or_never().is_some());
        assert!((&mut request.ready).now_or_never().unwrap().is_err());
        assert!(matches!(target.attach(), Err(AttachError::Stale)));
        let mut next = sessions.next_session();
        assert!(!next.accept_pause());
    }

    #[tokio::test]
    async fn executions_get_fresh_opaque_ids() {
        let (mut first_registration, first_handle) = InteractiveSessionFactory::for_pipeline();
        let (mut second_registration, second_handle) = InteractiveSessionFactory::for_pipeline();
        first_registration.set_run_id("same-run-id".into());
        second_registration.set_run_id("same-run-id".into());
        let first = first_registration.next_session();
        let second = second_registration.next_session();
        let first_id = first_handle.target().execution_id().unwrap().to_owned();
        let second_id = second_handle.target().execution_id().unwrap().to_owned();
        assert_ne!(first_id, second_id);
        assert!(!first_id.contains("same-run-id"));
        assert!(!second_id.contains("same-run-id"));
        assert_eq!(
            first_handle.target().nonce(),
            second_handle.target().nonce()
        );
        drop((first, second));
    }

    #[tokio::test]
    async fn correction_reservation_does_not_cross_turn_boundary() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let mut correction = handle.target().attach().unwrap();
        drop(first);
        assert!((&mut correction.ready).now_or_never().unwrap().is_err());
        let mut second = sessions.next_session();
        assert!(!second.accept_pause());
        assert!(handle.target().attach().is_ok());
    }

    #[tokio::test]
    async fn read_only_target_cannot_reserve_commands() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_pipeline();
        let _execution = sessions.next_session();
        let target = handle.target();
        let identity = target.execution_id().map(str::to_owned);
        let signal = target.end_signal();
        let passive = target.read_only();
        assert_eq!(passive.execution_id().map(str::to_owned), identity);
        assert_eq!(passive.end_signal().is_ended(), signal.is_ended());
        assert!(matches!(passive.attach(), Err(AttachError::Busy)));
        assert!(matches!(passive.attach_debugger(), Err(AttachError::Busy)));
        assert!(handle.target().attach().is_ok());
    }

    #[tokio::test]
    async fn rejected_read_only_handoff_can_be_withdrawn_and_reused() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut request = handle.attach_next_turn_debugger(None).unwrap();
        let mut readonly_successor = sessions.next_session_with_commands(false, None).unwrap();
        assert!((&mut request.ready).now_or_never().unwrap().is_err());
        assert!(!readonly_successor.accept_pause());

        // This must not deadlock by trying to acquire the reservation mutex
        // recursively through the attachment's owner route.
        drop(request);
        drop(readonly_successor);
        assert!(handle.attach_next_turn_debugger(None).is_ok());
    }

    #[tokio::test]
    async fn unsupported_backend_capability_is_captured_and_enforced() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_pipeline();
        let mut session = sessions.next_session_with_commands(false, None).unwrap();
        let snapshot = handle.target();
        assert!(matches!(snapshot.attach(), Err(AttachError::Busy)));
        assert!(matches!(snapshot.attach_debugger(), Err(AttachError::Busy)));
        assert!(!session.accept_pause());

        // Capability is fixed before publication and cannot be promoted by a
        // later session mutation.
        assert!(matches!(snapshot.attach(), Err(AttachError::Busy)));
        assert!(matches!(handle.target().attach(), Err(AttachError::Busy)));
    }

    #[tokio::test]
    async fn prepared_capability_mismatch_fails_without_replacing_identity() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let announced_id = sessions.prepare_next_with_commands(true, None);

        assert!(sessions.next_session_with_commands(false, None).is_err());
        assert_eq!(handle.target().execution_id(), Some(announced_id.as_str()));
        assert!(handle.end_signal().unwrap().is_ended());

        let successor_id = sessions.prepare_next_with_commands(false, None);
        assert_ne!(successor_id, announced_id);
        assert_eq!(handle.target().execution_id(), Some(successor_id.as_str()));
    }

    #[test]
    fn prepared_snapshot_belongs_to_its_factory_after_registration_replacement() {
        let (mut original, _) = InteractiveSessionFactory::for_chat();
        let (mut replacement, replacement_handle) = InteractiveSessionFactory::for_chat();
        let replacement_target = replacement.prepare_target_with_commands(true, None);
        let original_target = original.prepare_target_with_commands(true, None);

        assert_eq!(original_target.nonce(), replacement_target.nonce());
        assert_ne!(
            original_target.execution_id(),
            replacement_target.execution_id()
        );
        assert_eq!(
            replacement_handle.target().execution_id(),
            replacement_target.execution_id()
        );
        assert!(!original_target.end_signal().is_ended());
    }

    #[tokio::test]
    async fn bound_snapshot_follows_only_its_acquisition() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut lease = handle.attach_next_turn_debugger(None).unwrap();
        assert!(lease.bound_target().is_err());
        let mut first = sessions.next_session();
        assert!(first.accept_pause());
        let waiting = first.next_command();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        (&mut lease.ready).await.unwrap();
        let bound = lease.bound_target().unwrap();
        assert_eq!(bound.execution_id(), handle.target().execution_id());
        assert_eq!(bound.nonce(), handle.target().nonce());

        let mut second = sessions.next_session();
        let replacement = handle.target().attach_debugger().unwrap();
        assert!(second.accept_pause());
        assert!(lease.bound_target().is_err());
        assert!(bound.end_signal().is_ended());
        assert_ne!(bound.execution_id(), handle.target().execution_id());
        assert!(replacement.bound_target().is_ok());
    }

    #[tokio::test]
    async fn bound_snapshot_rejects_a_new_acquisition_on_the_same_execution() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut session = sessions.next_session();
        let old = handle.target().attach_debugger().unwrap();
        assert!(session.accept_pause());
        assert!(old.bound_target().is_ok());
        session.finish_attachment(DebugOutcome::Ended {
            reason: "released".into(),
        });
        let new = handle.target().attach_debugger().unwrap();
        assert!(session.accept_pause());
        assert!(old.bound_target().is_err());
        assert!(new.bound_target().is_ok());
    }

    #[tokio::test]
    async fn debugger_reservation_hands_off_to_chat_successor() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let debugger = handle.attach_next_turn_debugger(None).unwrap();
        let mut first = sessions.next_session();
        assert!(first.accept_pause());
        assert!(debugger.try_send(InteractiveCommand::RunTurn).is_ok());
        assert!(matches!(
            first.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
        first.finish_attachment(DebugOutcome::Ended {
            reason: "done".into(),
        });
        assert!(handle.target().attach_debugger().is_ok());
    }

    #[tokio::test]
    async fn replacement_cleanup_preserves_supervisor_terminal_and_successor_lease() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let terminal = sessions.supervise_next_turn();
        let mut first = sessions.next_session();
        let mut owner_a = handle.target().attach_debugger().unwrap();
        assert!(first.accept_pause());
        {
            let waiting = first.next_command();
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
            (&mut owner_a.ready).await.unwrap();

            let mut second = sessions.next_session();
            let owner_b = handle.target().attach_debugger().unwrap();
            assert!(second.accept_pause());
            assert!(waiting.as_mut().await.is_none());

            // A's agent cleanup races the wakeup, but its acquisition was
            // revoked. Dropping A's session runs its cleanup after revocation.
            assert!(matches!(
                handle.target().attach_debugger(),
                Err(AttachError::Busy)
            ));
            terminal.end("classified by supervisor");
            assert!(matches!((&mut owner_a.ended).await,
                Ok(DebugOutcome::Ended { reason }) if reason == "classified by supervisor"));

            owner_b.try_send(InteractiveCommand::RunTurn).unwrap();
            assert!(matches!(
                second.next_command().await,
                Some(InteractiveCommand::RunTurn)
            ));
        }
    }

    #[tokio::test]
    async fn retained_debugger_can_send_message_then_command_and_detach() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut session = sessions.next_session();
        let debugger = handle.target().attach_debugger().unwrap();
        assert!(session.accept_pause());

        debugger
            .try_send(InteractiveCommand::Message("correction".into(), None, None))
            .unwrap();
        let Some(InteractiveCommand::Message(_, _, _)) = session.next_command().await else {
            panic!("message should be delivered to the retained debugger");
        };
        session.finish_message_delivery();
        assert!(session.has_accepted_acquisition());

        debugger.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            session.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
        session.finish_attachment(DebugOutcome::Ended {
            reason: "detached".into(),
        });
        drop(debugger);
        assert!(handle.target().attach_debugger().is_ok());
    }

    #[tokio::test]
    async fn replacement_revokes_accepted_lease_before_first_command_poll() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut outgoing = sessions.next_session();
        let mut lease_a = handle.target().attach().unwrap();
        assert!(outgoing.accept_pause());
        lease_a.try_send(InteractiveCommand::RunTurn).unwrap();

        // Replace before A polls its inbox. Revocation must invalidate the
        // accepted acquisition, not merely signal its execution end.
        let mut successor = sessions.next_session();
        let lease_b = handle.target().attach().unwrap();
        assert!(successor.accept_pause());
        assert!((&mut lease_a.ready).now_or_never().is_none());
        assert!(outgoing.next_command().await.is_none());
        assert!((&mut lease_a.ready).now_or_never().is_none());
        lease_b.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            successor.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
    }

    #[tokio::test]
    async fn replacement_revokes_unaccepted_live_debugger_request() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let _old_session = sessions.next_session();
        let mut debugger = handle.target().attach_debugger().unwrap();

        // Keep both the old execution and its attachment alive while forcing a
        // replacement; the pending lease must not migrate to the successor.
        let mut successor = sessions.next_session();
        assert!((&mut debugger.ready).now_or_never().unwrap().is_err());
        assert!(!successor.accept_pause());
        assert!(handle.target().attach().is_ok());
    }

    #[tokio::test]
    async fn live_turn_rejects_between_turn_reservation_without_affecting_successor() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let live = sessions.next_session();
        assert!(matches!(
            handle.attach_next_turn_debugger(None),
            Err(AttachError::Busy)
        ));
        drop(live);
        let mut successor = sessions.next_session();
        assert!(!successor.accept_pause());
    }

    #[tokio::test]
    async fn debugger_reservation_owner_can_withdraw_before_and_after_handoff() {
        // The reservation owner can withdraw before the turn boundary.
        let (mut before_handoff, before_handle) = InteractiveSessionFactory::for_chat();
        let owner = before_handle.attach_next_turn_debugger(None).unwrap();
        drop(owner);
        let mut first = before_handoff.next_session();
        assert!(!first.accept_pause());

        // A writable turn can be minted while the reservation is pending.
        // The owner may withdraw after handoff but before acceptance.
        let (mut before_accept, before_accept_handle) = InteractiveSessionFactory::for_chat();
        let owner = before_accept_handle
            .attach_next_turn_debugger(None)
            .unwrap();
        let mut receiving_turn = before_accept.next_session();
        assert!(matches!(
            before_accept_handle.target().attach(),
            Err(AttachError::Busy)
        ));
        drop(owner);
        assert!(!receiving_turn.accept_pause());
        assert!(receiving_turn.next_command().await.is_none());
        receiving_turn.discard_revoked_inbox();
        let replacement = before_accept_handle.target().attach().unwrap();
        assert!(receiving_turn.accept_pause());
        replacement.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            receiving_turn.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));

        // Once handed off and accepted, detach still releases the turn for a
        // replacement attachment.
        let (mut after_handoff, after_handle) = InteractiveSessionFactory::for_chat();
        let owner = after_handle.attach_next_turn_debugger(None).unwrap();
        let mut receiving_turn = after_handoff.next_session();
        assert!(receiving_turn.accept_pause());
        drop(owner);
        assert!(receiving_turn.next_command().await.is_none());
        receiving_turn.discard_revoked_inbox();
        let replacement = after_handle.target().attach().unwrap();
        assert!(receiving_turn.accept_pause());
        replacement.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            receiving_turn.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
    }

    #[tokio::test]
    async fn debugger_detach_after_acceptance_releases_the_turn() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let terminal = sessions.supervise_next_turn();
        let mut owner = handle.attach_next_turn_debugger(None).unwrap();
        let mut session = sessions.next_session();
        assert!(session.accept_pause());
        {
            let waiting = session.next_command();
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
            (&mut owner.ready).await.unwrap();
            drop(owner);
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
                    .await
                    .expect("detach wakes the accepted command wait")
                    .is_none()
            );
        }
        session.discard_revoked_inbox();
        let replacement = handle.target().attach().unwrap();
        assert!(session.accept_pause());
        replacement.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            session.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
        terminal.end("done");
    }

    #[tokio::test]
    async fn revoked_accepted_inbox_cannot_deliver_buffered_command() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut session = sessions.next_session();
        let owner_a = handle.target().attach().unwrap();
        assert!(session.accept_pause());
        owner_a
            .try_send(InteractiveCommand::Bail("stale".into()))
            .unwrap();
        drop(owner_a);

        let owner_b = handle.target().attach().unwrap();
        assert!(session.next_command().await.is_none());
        assert!(matches!(handle.target().attach(), Err(AttachError::Busy)));

        // The old receiver cleanup must not clear B's accepted acquisition.
        assert!(session.accept_pause());
        owner_b.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            session.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
    }

    #[tokio::test]
    async fn delayed_old_lease_drop_cannot_clear_a_replacement() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut first = sessions.next_session();
        let old = handle.target().attach().unwrap();
        assert!(first.accept_pause());

        // Replacing the execution releases its accepted slot. The old owner
        // is deliberately retained to model delayed cleanup after replacement.
        let _replacement_execution = sessions.next_session();
        let replacement = handle.target().attach().unwrap();
        drop(old);
        assert!(matches!(handle.target().attach(), Err(AttachError::Busy)));
        drop(replacement);
        assert!(handle.target().attach().is_ok());
    }

    #[tokio::test]
    async fn delayed_old_lease_drop_cannot_clear_same_execution_replacement() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut session = sessions.next_session();
        let lease_a = handle.target().attach().unwrap();
        assert!(session.accept_pause());
        lease_a.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            session.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
        session.finish_attachment(DebugOutcome::Ended {
            reason: "done".into(),
        });

        let lease_b = handle.target().attach().unwrap();
        drop(lease_a);
        assert!(matches!(handle.target().attach(), Err(AttachError::Busy)));
        drop(lease_b);
        assert!(handle.target().attach().is_ok());
    }

    #[tokio::test]
    async fn delayed_old_lease_drop_cannot_clear_successor_pause() {
        let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
        let mut first = sessions.next_session();
        let old = handle.target().attach().unwrap();
        assert!(first.accept_pause());
        old.try_send(InteractiveCommand::RunTurn).unwrap();
        assert!(matches!(
            first.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));
        first.finish_attachment(DebugOutcome::Ended {
            reason: "done".into(),
        });

        let mut second = sessions.next_session();
        let replacement = handle.target().attach().unwrap();
        drop(old);
        assert!(matches!(handle.target().attach(), Err(AttachError::Busy)));
        assert!(second.accept_pause());
        drop(replacement);
        assert!(handle.target().attach().is_ok());
    }

    #[tokio::test]
    async fn ended_target_only_allows_chat_debugger_reservation() {
        let (mut chat, chat_handle) = InteractiveSessionFactory::for_chat();
        let execution = chat.next_session();
        let ended_target = chat_handle.target();
        drop(execution);
        assert!(matches!(ended_target.attach(), Err(AttachError::Stale)));
        assert!(matches!(
            ended_target.attach_debugger(),
            Err(AttachError::Stale)
        ));
        let debugger = chat_handle
            .attach_next_turn_debugger(ended_target.execution_id.clone())
            .unwrap();
        let mut next = chat.next_session();
        assert!(next.accept_pause());
        assert!(debugger.try_send(InteractiveCommand::RunTurn).is_ok());
        assert!(matches!(
            next.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));

        let (mut pipeline, pipeline_handle) = InteractiveSessionFactory::for_pipeline();
        let execution = pipeline.next_session();
        let ended_target = pipeline_handle.target();
        drop(execution);
        assert!(matches!(ended_target.attach(), Err(AttachError::Stale)));
        assert!(matches!(
            ended_target.attach_debugger(),
            Err(AttachError::Stale)
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

        let mut second = sessions.next_session();
        let next = handle.target().attach().unwrap();
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
