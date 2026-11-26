//! Execution-scoped input and observation. Sender lifetime never changes pause state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::clients::cancel::{CancelListener, CancelToken};
use crate::clients::execution_input::{
    ExecutionIdentity, InputCommand, InputReceiver, InputRegistry, InputRevoker, InputSender,
};
use tokio::sync::{mpsc, Notify};

#[derive(Clone, Debug, Default)]
pub(crate) struct EndSignal {
    ended: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl EndSignal {
    fn signal(&self) {
        self.ended.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    pub(crate) async fn wait(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_ended() {
            notified.await;
        }
    }
}

/// Serializes late input binding with cancellation, replacement and teardown.
#[derive(Debug, Default)]
struct ExecutionLifetime {
    end: EndSignal,
    revoker: Mutex<Option<Arc<InputRevoker>>>,
}

impl ExecutionLifetime {
    fn end(&self) {
        let revoker = self.revoker.lock().unwrap_or_else(|e| e.into_inner());
        self.end.signal();
        if let Some(revoker) = revoker.as_ref() {
            revoker.revoke();
        }
    }
}

impl CancelListener for ExecutionLifetime {
    fn on_cancel(&self) {
        self.end();
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InteractiveHandle {
    target: Arc<Mutex<DebugTarget>>,
    inputs: Arc<Mutex<InputRegistry>>,
}

impl InteractiveHandle {
    pub(crate) fn set_input_registry(&self, registry: InputRegistry) {
        *self.inputs.lock().unwrap() = registry;
    }

    #[cfg(test)]
    pub(crate) fn input_sender(&self, identity: &ExecutionIdentity) -> Option<InputSender> {
        self.inputs.lock().unwrap().sender(identity)
    }

    pub(crate) fn resolve_input(
        &self,
        run_id: &str,
        stage_run_id: &str,
        stage: Option<&str>,
    ) -> Option<(ExecutionIdentity, InputSender)> {
        self.inputs
            .lock()
            .unwrap()
            .resolve(run_id, stage_run_id, stage)
    }

    pub(crate) fn target(&self) -> DebugTarget {
        self.target.lock().unwrap().clone()
    }
}

/// Immutable observation snapshot, not a debugger reservation or command lease.
#[derive(Clone, Debug, Default)]
pub(crate) struct DebugTarget {
    nonce: u64,
    stage_run_id: Option<String>,
    commands_supported: bool,
    end_signal: EndSignal,
}

impl DebugTarget {
    #[cfg(test)]
    pub(crate) fn nonce(&self) -> u64 {
        self.nonce
    }

    pub(crate) fn stage_run_id(&self) -> Option<&str> {
        self.stage_run_id.as_deref()
    }

    pub(crate) fn end_signal(&self) -> EndSignal {
        self.end_signal.clone()
    }

    pub(crate) fn accepts_commands(&self) -> bool {
        self.commands_supported
    }
}

#[derive(Debug)]
pub(crate) struct InteractiveSessionFactory {
    handle: InteractiveHandle,
    current: Option<Arc<ExecutionLifetime>>,
    next_nonce: u64,
    prepared: Option<InteractiveSession>,
}

impl Drop for InteractiveSessionFactory {
    fn drop(&mut self) {
        self.handle.target().end_signal.signal();
        if let Some(current) = &self.current {
            current.end();
        }
    }
}

impl InteractiveSessionFactory {
    pub(crate) fn for_pipeline() -> (Self, InteractiveHandle) {
        let handle = InteractiveHandle::default();
        (
            Self {
                handle: handle.clone(),
                current: None,
                next_nonce: 1,
                prepared: None,
            },
            handle,
        )
    }

    pub(crate) fn for_chat() -> (Self, InteractiveHandle) {
        Self::for_pipeline()
    }

    /// Run identity is supplied separately when binding exact execution input.
    pub(crate) fn set_run_id(&self, _id: String) {}

    #[cfg(test)]
    pub(crate) fn next_session(&mut self) -> InteractiveSession {
        self.next_session_with_commands(true, None).unwrap()
    }

    pub(crate) fn next_session_with_commands(
        &mut self,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<InteractiveSession, &'static str> {
        self.next_session_with_identity(None, accepts_commands, cancel)
    }

    pub(crate) fn next_session_for_stage(
        &mut self,
        stage_run_id: &str,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<InteractiveSession, &'static str> {
        self.next_session_with_identity(Some(stage_run_id), accepts_commands, cancel)
    }

    fn next_session_with_identity(
        &mut self,
        stage_run_id: Option<&str>,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> Result<InteractiveSession, &'static str> {
        if let Some(session) = self.prepared.take() {
            if stage_run_id.is_some_and(|id| session.target.stage_run_id() != Some(id)) {
                return Err("interactive session identity disagrees with stage invocation");
            }
            if session.accepts_commands() != accepts_commands {
                return Err("prepared interactive session capability disagrees with backend");
            }
            if let Some(cancel) = cancel {
                cancel.listen(session.lifetime.clone());
            }
            return Ok(session);
        }
        Ok(self.mint(stage_run_id, accepts_commands, cancel))
    }

    pub(crate) fn discard_prepared(&mut self) {
        drop(self.prepared.take());
    }

    pub(crate) fn prepare_next_with_commands(
        &mut self,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> String {
        let session = self.mint(None, accepts_commands, cancel);
        let id = session.target.stage_run_id.clone().unwrap();
        self.prepared = Some(session);
        id
    }

    #[cfg(test)]
    pub(crate) fn prepare_target_with_commands(
        &mut self,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> DebugTarget {
        self.prepare_next_with_commands(accepts_commands, cancel);
        self.handle.target()
    }

    fn mint(
        &mut self,
        stage_run_id: Option<&str>,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) -> InteractiveSession {
        self.handle.target().end_signal.signal();
        if let Some(previous) = self.current.take() {
            previous.end();
        }
        let lifetime = Arc::new(ExecutionLifetime::default());
        let target = DebugTarget {
            nonce: self.next_nonce,
            stage_run_id: Some(
                stage_run_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("exec-{}", uuid::Uuid::new_v4())),
            ),
            commands_supported: accepts_commands,
            end_signal: lifetime.end.clone(),
        };
        self.next_nonce = self.next_nonce.wrapping_add(1);
        if let Some(cancel) = cancel {
            cancel.listen(lifetime.clone());
        }
        *self.handle.target.lock().unwrap() = target.clone();
        self.current = Some(lifetime.clone());
        InteractiveSession {
            lifetime,
            target,
            input_identity: None,
            input: None,
            inbox_paused: false,
            inbox_steps: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct InteractiveSession {
    input_identity: Option<ExecutionIdentity>,
    input: Option<InputReceiver>,
    inbox_paused: bool,
    inbox_steps: usize,
    lifetime: Arc<ExecutionLifetime>,
    target: DebugTarget,
}

impl Drop for InteractiveSession {
    fn drop(&mut self) {
        self.lifetime.end();
    }
}

impl InteractiveSession {
    pub(crate) fn bind_input(
        &mut self,
        registry: &InputRegistry,
        identity: ExecutionIdentity,
        accepts_commands: bool,
        cancel: Option<&CancelToken>,
    ) {
        assert!(
            self.input_identity.is_none(),
            "execution input already bound"
        );
        self.input_identity = Some(identity.clone());
        if !accepts_commands {
            return;
        }
        let alias = self
            .target
            .stage_run_id
            .clone()
            .filter(|id| id != &identity.stage_run_id)
            .map(|stage_run_id| ExecutionIdentity {
                stage_run_id,
                ..identity.clone()
            });
        let (receiver, _) =
            registry.register_with_aliases(identity, alias, Some(self.target.nonce), |revoker| {
                if let Some(cancel) = cancel {
                    cancel.listen(revoker.clone());
                }
                let mut bound = self
                    .lifetime
                    .revoker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *bound = Some(revoker.clone());
                if self.lifetime.end.is_ended() {
                    revoker.revoke();
                }
            });
        self.input = Some(receiver);
    }

    pub(crate) fn try_recv_input(&mut self) -> Result<InputCommand, mpsc::error::TryRecvError> {
        self.input
            .as_mut()
            .ok_or(mpsc::error::TryRecvError::Disconnected)?
            .try_recv()
    }

    pub(crate) fn input_pending(&self) -> bool {
        self.input.as_ref().is_some_and(InputReceiver::pending)
    }

    pub(crate) async fn input_requested(&self) {
        tokio::select! {
            _ = self.wait_execution_end() => {},
            _ = async {
                match &self.input {
                    Some(input) => input.wait_pending().await,
                    None => std::future::pending().await,
                }
            } => {},
        }
    }

    #[cfg(test)]
    pub(crate) async fn next_input(&mut self) -> Option<InputCommand> {
        let input = self.input.as_mut()?;
        tokio::select! {
            biased;
            _ = self.lifetime.end.wait() => None,
            command = input.recv() => command,
        }
    }

    pub(crate) async fn wait_execution_end(&self) {
        self.lifetime.end.wait().await;
    }

    pub(crate) async fn stream_interrupted(&self) -> bool {
        self.input_requested().await;
        !self.execution_ended()
    }

    pub(crate) fn execution_ended(&self) -> bool {
        self.lifetime.end.is_ended()
    }

    pub(crate) fn grant_inbox_step(&mut self) {
        self.inbox_steps += 1;
    }

    pub(crate) fn take_inbox_step(&mut self) -> bool {
        if self.inbox_steps == 0 {
            false
        } else {
            self.inbox_steps -= 1;
            true
        }
    }

    pub(crate) fn clear_inbox_steps(&mut self) {
        self.inbox_steps = 0;
    }
    pub(crate) fn inbox_paused(&self) -> bool {
        self.inbox_paused
    }
    pub(crate) fn set_inbox_paused(&mut self, paused: bool) {
        self.inbox_paused = paused;
    }
    pub fn unattached() -> Self {
        Self::default()
    }
    pub(crate) fn snapshot(&self) -> DebugTarget {
        self.target.clone()
    }
    pub(crate) fn accepts_commands(&self) -> bool {
        self.target.commands_supported
    }
}

#[cfg(test)]
#[path = "interactive/input_tests.rs"]
mod input_tests;
