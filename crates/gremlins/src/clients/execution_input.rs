//! Execution-scoped command inboxes, independent of debugger attachments.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{mpsc, oneshot, Notify};

pub(crate) const INPUT_CAPACITY: usize = 16;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ExecutionIdentity {
    pub(crate) run_id: String,
    pub(crate) stage_name: String,
    pub(crate) stage_run_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Delivery {
    Delivered,
    NotDelivered,
}

pub(crate) type Receipt = oneshot::Receiver<Delivery>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum InputRejected {
    #[error("execution input queue is full")]
    Full,
    #[error("execution input is revoked")]
    Revoked,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InputRegistry {
    entries: Arc<Mutex<HashMap<ExecutionIdentity, Weak<Shared>>>>,
}

impl InputRegistry {
    #[cfg(test)]
    pub(crate) fn register(
        &self,
        identity: ExecutionIdentity,
        initialize: impl FnOnce(Arc<InputRevoker>),
    ) -> (InputReceiver, Arc<InputRevoker>) {
        self.register_with_nonce(identity, None, initialize)
    }

    /// Panics if this identity already has a live, unrevoked inbox.
    #[cfg(test)]
    pub(crate) fn register_with_nonce(
        &self,
        identity: ExecutionIdentity,
        nonce: Option<u64>,
        initialize: impl FnOnce(Arc<InputRevoker>),
    ) -> (InputReceiver, Arc<InputRevoker>) {
        self.register_with_aliases(identity, [], nonce, initialize)
    }

    /// Publish all routes atomically; every route resolves to the canonical identity.
    pub(crate) fn register_with_aliases(
        &self,
        identity: ExecutionIdentity,
        aliases: impl IntoIterator<Item = ExecutionIdentity>,
        nonce: Option<u64>,
        initialize: impl FnOnce(Arc<InputRevoker>),
    ) -> (InputReceiver, Arc<InputRevoker>) {
        let routes: Vec<_> = std::iter::once(identity.clone()).chain(aliases).collect();
        let (sender, receiver) = mpsc::channel(INPUT_CAPACITY);
        let shared = Arc::new(Shared {
            identity,
            nonce,
            pending: Notify::new(),
            closed: Notify::new(),
            state: Mutex::new(State {
                sender: Some(sender),
                next_id: 0,
                pending: HashMap::new(),
            }),
        });
        let revoker = Arc::new(InputRevoker {
            shared: shared.clone(),
        });
        // Install lifecycle guards before any caller can discover the inbox.
        initialize(revoker.clone());
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, entry| entry.strong_count() != 0);
        for route in &routes {
            if let Some(existing) = entries.get(route).and_then(Weak::upgrade) {
                assert!(
                    existing.state.lock().unwrap().sender.is_none(),
                    "execution input identity is already registered: {route:?}"
                );
            }
        }
        for route in routes {
            entries.insert(route, Arc::downgrade(&shared));
        }
        (
            InputReceiver {
                shared: shared.clone(),
                receiver,
            },
            revoker,
        )
    }

    /// Resolve only live inboxes; debugger snapshots are not routing authority.
    pub(crate) fn resolve(
        &self,
        run_id: &str,
        stage_run_id: &str,
        stage: Option<&str>,
    ) -> Option<(ExecutionIdentity, InputSender)> {
        let entries = self.entries.lock().unwrap();
        let mut matches = entries.iter().filter_map(|(identity, entry)| {
            if identity.run_id != run_id
                || identity.stage_run_id != stage_run_id
                || stage.is_some_and(|stage| identity.stage_name != stage)
            {
                return None;
            }
            let shared = entry.upgrade()?;
            let active = shared.state.lock().unwrap().sender.is_some();
            active.then(|| (shared.identity.clone(), InputSender { shared }))
        });
        let matched = matches.next()?;
        // Never select an arbitrary inbox when an incomplete identity is ambiguous.
        matches.next().is_none().then_some(matched)
    }

    pub(crate) fn sender(&self, identity: &ExecutionIdentity) -> Option<InputSender> {
        let shared = self.entries.lock().unwrap().get(identity)?.upgrade()?;
        let active = shared.state.lock().unwrap().sender.is_some();
        active.then_some(InputSender { shared })
    }
}

#[derive(Debug)]
struct Shared {
    identity: ExecutionIdentity,
    nonce: Option<u64>,
    pending: Notify,
    closed: Notify,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    // Taking this sender is the revocation linearization point.
    sender: Option<mpsc::Sender<Queued>>,
    next_id: u64,
    pending: HashMap<u64, oneshot::Sender<Delivery>>,
}

impl Shared {
    fn revoke(&self) {
        let mut state = self.state.lock().unwrap();
        state.sender.take();
        for (_, receipt) in state.pending.drain() {
            let _ = receipt.send(Delivery::NotDelivered);
        }
        drop(state);
        self.closed.notify_waiters();
    }

    fn settle(&self, id: u64, delivery: Delivery) {
        let mut state = self.state.lock().unwrap();
        if let Some(receipt) = state.pending.remove(&id) {
            let _ = receipt.send(delivery);
        }
    }
}

#[derive(Debug)]
struct Queued {
    id: u64,
    request_id: String,
    payload: Payload,
}

#[derive(Debug)]
enum Payload {
    Correction(String),
    Control(InputControl),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputControl {
    Pause,
    Step,
    Resume,
    Bail,
}

#[derive(Debug)]
pub(crate) enum InputCommand {
    Correction(Correction),
    Control(Control),
}

#[derive(Clone, Debug)]
pub(crate) struct InputSender {
    shared: Arc<Shared>,
}

impl InputSender {
    /// The invocation nonce captured when this exact inbox was registered.
    pub(crate) fn nonce(&self) -> Option<u64> {
        self.shared.nonce
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.state.lock().unwrap().sender.is_none()
    }

    pub(crate) async fn closed(&self) {
        loop {
            let notified = self.shared.closed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }

    /// Acceptance means enqueued, not yet incorporated into the conversation.
    pub(crate) fn try_send(
        &self,
        request_id: String,
        text: String,
    ) -> Result<Receipt, InputRejected> {
        self.enqueue(request_id, Payload::Correction(text))
    }

    pub(crate) fn try_control(
        &self,
        request_id: String,
        control: InputControl,
    ) -> Result<Receipt, InputRejected> {
        self.enqueue(request_id, Payload::Control(control))
    }

    fn enqueue(&self, request_id: String, payload: Payload) -> Result<Receipt, InputRejected> {
        let mut state = self.shared.state.lock().unwrap();
        let sender = state.sender.as_ref().ok_or(InputRejected::Revoked)?;
        let id = state.next_id;
        let next_id = id.checked_add(1).expect("execution input id exhausted");
        sender
            .try_send(Queued {
                id,
                request_id,
                payload,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InputRejected::Full,
                mpsc::error::TrySendError::Closed(_) => InputRejected::Revoked,
            })?;
        let (completion, receipt) = oneshot::channel();
        state.next_id = next_id;
        state.pending.insert(id, completion);
        self.shared.pending.notify_waiters();
        Ok(receipt)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct InputRevoker {
    shared: Arc<Shared>,
}

impl crate::clients::cancel::CancelListener for InputRevoker {
    fn on_cancel(&self) {
        self.revoke();
    }
}

impl InputRevoker {
    /// Settles queued and already-polled corrections without consumer progress.
    pub(crate) fn revoke(&self) {
        self.shared.revoke();
    }
}

#[derive(Debug)]
pub(crate) struct InputReceiver {
    shared: Arc<Shared>,
    receiver: mpsc::Receiver<Queued>,
}

impl InputReceiver {
    pub(crate) fn pending(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.sender.is_some() && !self.receiver.is_empty()
    }

    /// Wait only for queued input. Revocation and endpoint drops do not wake this wait.
    pub(crate) async fn wait_pending(&self) {
        loop {
            let notified = self.shared.pending.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending() {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn recv(&mut self) -> Option<InputCommand> {
        self.shared.state.lock().unwrap().sender.as_ref()?;
        // Channel closure wakes this receive on revocation, unlike wait_pending,
        // which must only interrupt a stream for accepted input.
        let queued = self.receiver.recv().await?;
        self.command(queued).ok()
    }

    pub(crate) fn try_recv(&mut self) -> Result<InputCommand, mpsc::error::TryRecvError> {
        let state = self.shared.state.lock().unwrap();
        if state.sender.is_none() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        drop(state);
        let queued = self.receiver.try_recv()?;
        self.command(queued)
    }

    fn command(&self, queued: Queued) -> Result<InputCommand, mpsc::error::TryRecvError> {
        let state = self.shared.state.lock().unwrap();
        if state.sender.is_none() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        Ok(match queued.payload {
            Payload::Correction(text) => InputCommand::Correction(Correction {
                request_id: queued.request_id,
                text,
                id: queued.id,
                shared: self.shared.clone(),
            }),
            Payload::Control(control) => InputCommand::Control(Control {
                request_id: queued.request_id,
                control,
                id: queued.id,
                shared: self.shared.clone(),
            }),
        })
    }
}

impl Drop for InputReceiver {
    fn drop(&mut self) {
        self.shared.revoke();
    }
}

#[derive(Debug)]
pub(crate) struct Correction {
    pub(crate) request_id: String,
    pub(crate) text: String,
    id: u64,
    shared: Arc<Shared>,
}

impl Correction {
    /// Apply and acknowledge atomically with respect to revocation.
    /// Returns false without calling `apply` if the request was revoked.
    /// `apply` must be synchronous and must not re-enter this inbox or cancel it.
    pub(crate) fn incorporate(self, apply: impl FnOnce(&str, &str)) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        if !state.pending.contains_key(&self.id) {
            return false;
        }
        apply(&self.request_id, &self.text);
        if let Some(receipt) = state.pending.remove(&self.id) {
            let _ = receipt.send(Delivery::Delivered);
        }
        true
    }
}

impl Drop for Correction {
    fn drop(&mut self) {
        self.shared.settle(self.id, Delivery::NotDelivered);
    }
}

#[derive(Debug)]
pub(crate) struct Control {
    pub(crate) request_id: String,
    pub(crate) control: InputControl,
    id: u64,
    shared: Arc<Shared>,
}

impl Control {
    /// Apply and acknowledge atomically with respect to revocation.
    /// `apply` must not re-enter this inbox or cancel it.
    pub(crate) fn incorporate(self, apply: impl FnOnce(&str, InputControl)) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        if !state.pending.contains_key(&self.id) {
            return false;
        }
        apply(&self.request_id, self.control);
        if let Some(receipt) = state.pending.remove(&self.id) {
            let _ = receipt.send(Delivery::Delivered);
        }
        true
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.shared.settle(self.id, Delivery::NotDelivered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    #[derive(Default)]
    struct Flag(AtomicBool);

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn receive(receiver: &mut InputReceiver) -> Correction {
        let InputCommand::Correction(correction) = receiver.try_recv().unwrap() else {
            panic!("expected correction");
        };
        correction
    }

    fn identity(stage_run_id: &str) -> ExecutionIdentity {
        ExecutionIdentity {
            run_id: "run".into(),
            stage_name: "stage".into(),
            stage_run_id: stage_run_id.into(),
        }
    }

    #[tokio::test]
    async fn aliases_share_delivery_revocation_and_never_follow_replacement() {
        let registry = InputRegistry::default();
        let canonical = identity("invocation");
        let alias = identity("prepared");
        let (mut inbox, revoke) =
            registry.register_with_aliases(canonical.clone(), [alias.clone()], Some(7), |_| {});
        let (resolved, exact) = registry
            .resolve("run", "invocation", Some("stage"))
            .unwrap();
        let (aliased, prepared) = registry.resolve("run", "prepared", None).unwrap();
        assert_eq!(resolved, canonical);
        assert_eq!(aliased, canonical);
        assert!(Arc::ptr_eq(&exact.shared, &prepared.shared));
        assert!(registry.resolve("other", "prepared", None).is_none());
        assert!(registry.resolve("run", "prepared", Some("other")).is_none());
        let delivered = prepared.try_send("one".into(), "hello".into()).unwrap();
        assert!(receive(&mut inbox).incorporate(|_, text| assert_eq!(text, "hello")));
        assert_eq!(delivered.await.unwrap(), Delivery::Delivered);
        let held = exact.try_send("held".into(), "old".into()).unwrap();
        let correction = receive(&mut inbox);
        let queued = prepared.try_send("queued".into(), "old".into()).unwrap();
        revoke.revoke();
        assert_eq!(held.await.unwrap(), Delivery::NotDelivered);
        assert_eq!(queued.await.unwrap(), Delivery::NotDelivered);
        assert!(!correction.incorporate(|_, _| panic!("revoked input applied")));
        assert!(registry.resolve("run", "invocation", None).is_none());
        assert!(registry.resolve("run", "prepared", None).is_none());
        let successor = identity("successor");
        let (mut next, _revoke) =
            registry.register_with_aliases(successor.clone(), [alias], Some(8), |_| {});
        assert_eq!(
            registry.resolve("run", "prepared", None).unwrap().0,
            successor
        );
        for stale in [exact, prepared] {
            assert!(stale.is_closed());
            assert_eq!(
                stale.try_send("late".into(), "old".into()).unwrap_err(),
                InputRejected::Revoked
            );
        }
        drop(inbox);
        assert!(matches!(
            next.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(registry.resolve("run", "prepared", None).is_some());
    }

    #[tokio::test]
    async fn controls_are_distinct_and_receipted() {
        let registry = InputRegistry::default();
        let (mut receiver, revoker) = registry.register(identity("controls"), |_| {});
        let sender = registry.sender(&identity("controls")).unwrap();
        for control in [
            InputControl::Pause,
            InputControl::Step,
            InputControl::Resume,
            InputControl::Bail,
        ] {
            let receipt = sender.try_control("request".into(), control).unwrap();
            assert!(receiver.pending());
            receiver.wait_pending().await;
            let InputCommand::Control(command) = receiver.recv().await.unwrap() else {
                panic!("expected control");
            };
            assert!(!receiver.pending());
            assert!(command.incorporate(|id, value| {
                assert_eq!(id, "request");
                assert_eq!(value, control);
            }));
            assert_eq!(receipt.await.unwrap(), Delivery::Delivered);
        }
        let dropped = sender
            .try_control("drop".into(), InputControl::Pause)
            .unwrap();
        drop(receiver.try_recv().unwrap());
        assert_eq!(dropped.await.unwrap(), Delivery::NotDelivered);
        let revoked = sender
            .try_control("revoke".into(), InputControl::Bail)
            .unwrap();
        let InputCommand::Control(command) = receiver.try_recv().unwrap() else {
            panic!("expected control");
        };
        revoker.revoke();
        assert!(!command.incorporate(|_, _| panic!("revoked control applied")));
        assert_eq!(revoked.await.unwrap(), Delivery::NotDelivered);
    }

    #[test]
    fn pending_wakes_only_on_accepted_sends() {
        let registry = InputRegistry::default();
        let (mut receiver, revoker) = registry.register(identity("wake"), |_| {});
        let sender = registry.sender(&identity("wake")).unwrap();
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let wait = receiver.wait_pending();
            tokio::pin!(wait);
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            drop(sender.clone());
            assert!(!flag.0.load(Ordering::SeqCst));
            let receipt = sender.try_send("correction".into(), "text".into()).unwrap();
            assert!(flag.0.swap(false, Ordering::SeqCst));
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(()));
            drop(receipt);
        }
        drop(receiver.try_recv().unwrap());
        let shared = receiver.shared.clone();
        let notification = shared.pending.notified();
        tokio::pin!(notification);
        assert!(notification.as_mut().poll(&mut cx).is_pending());
        for _ in 0..INPUT_CAPACITY {
            sender
                .try_control("fill".into(), InputControl::Pause)
                .unwrap();
        }
        assert!(flag.0.swap(false, Ordering::SeqCst));
        assert!(notification.as_mut().poll(&mut cx).is_ready());
        let notification = shared.pending.notified();
        tokio::pin!(notification);
        assert!(notification.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            sender
                .try_control("full".into(), InputControl::Step)
                .unwrap_err(),
            InputRejected::Full
        );
        revoker.revoke();
        assert_eq!(
            sender
                .try_control("late".into(), InputControl::Resume)
                .unwrap_err(),
            InputRejected::Revoked
        );
        drop(sender);
        drop(receiver);
        drop(revoker);
        assert!(!flag.0.load(Ordering::SeqCst));
        assert!(notification.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn recv_wakes_on_revocation_and_rejects_buffered_input() {
        // Cover both an empty channel and input arriving before revocation but
        // not yet observed by the receiver.
        for queued in [false, true] {
            let registry = InputRegistry::default();
            let (mut receiver, revoker) = registry.register(identity("recv"), |_| {});
            let sender = registry.sender(&identity("recv")).unwrap();
            let flag = Arc::new(Flag::default());
            let waker = Waker::from(flag.clone());
            let mut cx = Context::from_waker(&waker);
            {
                let receive = receiver.recv();
                tokio::pin!(receive);
                assert!(receive.as_mut().poll(&mut cx).is_pending());
                let receipt =
                    queued.then(|| sender.try_send("queued".into(), "text".into()).unwrap());
                revoker.revoke();
                assert!(flag.0.load(Ordering::SeqCst));
                assert!(matches!(receive.as_mut().poll(&mut cx), Poll::Ready(None)));
                if let Some(mut receipt) = receipt {
                    assert_eq!(receipt.try_recv(), Ok(Delivery::NotDelivered));
                }
            }
            // Revocation before the first poll must also return immediately.
            let receive = receiver.recv();
            tokio::pin!(receive);
            assert!(matches!(receive.as_mut().poll(&mut cx), Poll::Ready(None)));
        }
    }

    #[test]
    fn wait_pending_stays_asleep_on_revocation() {
        let registry = InputRegistry::default();
        let (receiver, revoker) = registry.register(identity("pending"), |_| {});
        let sender = registry.sender(&identity("pending")).unwrap();
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        let wait = receiver.wait_pending();
        tokio::pin!(wait);
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        revoker.revoke();
        drop(sender);
        assert!(!flag.0.load(Ordering::SeqCst));
        assert!(wait.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn registration_is_hidden_until_cancellation_guard_is_installed() {
        use crate::clients::cancel::CancelToken;

        for cancel_before_registration in [false, true] {
            let registry = InputRegistry::default();
            let id = identity("cancelled");
            let cancel = CancelToken::new();
            if cancel_before_registration {
                cancel.cancel();
            }
            let (mut receiver, revoker) = registry.register(id.clone(), |revoker| {
                assert!(registry.sender(&id).is_none());
                cancel.listen(revoker);
                if !cancel_before_registration {
                    cancel.cancel();
                }
                assert!(registry.sender(&id).is_none());
            });
            assert!(registry.sender(&id).is_none());
            let retained = InputSender {
                shared: revoker.shared.clone(),
            };
            assert!(matches!(
                retained.try_send("late".into(), "rejected".into()),
                Err(InputRejected::Revoked)
            ));
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }

    #[test]
    fn dropping_one_sender_preserves_queue_and_other_sender() {
        let registry = InputRegistry::default();
        let (mut receiver, _revoker) = registry.register(identity("one"), |_| {});
        let first = registry.sender(&identity("one")).unwrap();
        let second = first.clone();
        let mut receipt = first
            .try_send("request".into(), "correction".into())
            .unwrap();
        drop(first);
        let correction = receive(&mut receiver);
        assert_eq!(correction.request_id, "request");
        assert_eq!(correction.text, "correction");
        assert!(correction.incorporate(|request_id, text| {
            assert_eq!(request_id, "request");
            assert_eq!(text, "correction");
        }));
        assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
        assert!(second.try_send("next".into(), "text".into()).is_ok());
    }

    #[test]
    fn receipts_complete_independently_even_with_duplicate_request_ids() {
        let registry = InputRegistry::default();
        let (mut receiver, _revoker) = registry.register(identity("one"), |_| {});
        let sender = registry.sender(&identity("one")).unwrap();
        let mut first = sender.try_send("same".into(), "first".into()).unwrap();
        let mut second = sender.try_send("same".into(), "second".into()).unwrap();
        let correction = receive(&mut receiver);
        receive(&mut receiver).incorporate(|_, _| {});
        assert_eq!(second.try_recv(), Ok(Delivery::Delivered));
        assert_eq!(first.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        drop(correction);
        assert_eq!(first.try_recv(), Ok(Delivery::NotDelivered));
    }

    #[test]
    fn capacity_rejection_does_not_accept_or_overwrite() {
        let registry = InputRegistry::default();
        let (mut receiver, _revoker) = registry.register(identity("one"), |_| {});
        let sender = registry.sender(&identity("one")).unwrap();
        let mut receipts = Vec::new();
        for i in 0..INPUT_CAPACITY {
            receipts.push(sender.try_send(i.to_string(), "text".into()).unwrap());
        }
        assert_eq!(
            sender
                .try_send("overflow".into(), "text".into())
                .unwrap_err(),
            InputRejected::Full
        );
        for i in 0..INPUT_CAPACITY {
            let correction = receive(&mut receiver);
            assert_eq!(correction.request_id, i.to_string());
            correction.incorporate(|_, _| {});
        }
        for receipt in &mut receipts {
            assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
        }
        assert!(sender.try_send("room".into(), "text".into()).is_ok());
    }

    #[test]
    fn revoke_settles_queued_and_in_flight_but_preserves_delivery() {
        let registry = InputRegistry::default();
        let (mut receiver, revoker) = registry.register(identity("one"), |_| {});
        let sender = registry.sender(&identity("one")).unwrap();
        let clone = sender.clone();
        let mut delivered = sender.try_send("done".into(), "text".into()).unwrap();
        receive(&mut receiver).incorporate(|_, _| {});
        let mut in_flight = sender.try_send("polled".into(), "text".into()).unwrap();
        let correction = receive(&mut receiver);
        let mut queued = sender.try_send("queued".into(), "text".into()).unwrap();
        revoker.clone().revoke();
        assert_eq!(in_flight.try_recv(), Ok(Delivery::NotDelivered));
        assert_eq!(queued.try_recv(), Ok(Delivery::NotDelivered));
        assert!(!correction.incorporate(|_, _| panic!("revoked input applied")));
        revoker.revoke();
        assert_eq!(delivered.try_recv(), Ok(Delivery::Delivered));
        assert_eq!(
            clone.try_send("late".into(), "text".into()).unwrap_err(),
            InputRejected::Revoked
        );
        assert!(registry.sender(&identity("one")).is_none());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn receiver_drop_revokes_polled_corrections() {
        let registry = InputRegistry::default();
        let (mut receiver, _revoker) = registry.register(identity("one"), |_| {});
        let sender = registry.sender(&identity("one")).unwrap();
        let mut receipt = sender.try_send("request".into(), "text".into()).unwrap();
        let correction = receive(&mut receiver);
        drop(receiver);
        assert_eq!(receipt.try_recv(), Ok(Delivery::NotDelivered));
        assert!(!correction.incorporate(|_, _| panic!("revoked input applied")));
        assert_eq!(
            sender.try_send("late".into(), "text".into()).unwrap_err(),
            InputRejected::Revoked
        );
    }

    #[test]
    fn incorporation_commits_before_concurrent_revocation() {
        let registry = InputRegistry::default();
        let (mut receiver, revoker) = registry.register(identity("one"), |_| {});
        let sender = registry.sender(&identity("one")).unwrap();
        let mut receipt = sender.try_send("request".into(), "text".into()).unwrap();
        let correction = receive(&mut receiver);
        let (started, revoking) = std::sync::mpsc::channel();
        let mut conversation = String::new();
        std::thread::scope(|scope| {
            assert!(correction.incorporate(|request_id, text| {
                assert_eq!(request_id, "request");
                assert!(matches!(
                    revoker.shared.state.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                scope.spawn(|| {
                    started.send(()).unwrap();
                    revoker.revoke();
                });
                revoking.recv().unwrap();
                conversation.push_str(text);
            }));
        });
        assert_eq!(conversation, "text");
        assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
        assert!(registry.sender(&identity("one")).is_none());
    }

    #[test]
    fn registry_does_not_keep_endpoints_alive() {
        let registry = InputRegistry::default();
        let (receiver, revoker) = registry.register(identity("one"), |_| {});
        let weak = Arc::downgrade(&revoker.shared);
        drop(receiver);
        drop(revoker);
        assert!(weak.upgrade().is_none());
        assert!(registry.sender(&identity("one")).is_none());
        let (_receiver, _revoker) = registry.register(identity("one"), |_| {});
    }

    #[test]
    fn resolve_matches_exact_fields_and_keeps_concurrent_inboxes() {
        let registry = InputRegistry::default();
        let (mut first, revoker) = registry.register(identity("one"), |_| {});
        let (mut second, _revoker) = registry.register(identity("two"), |_| {});
        assert!(registry.resolve("other", "one", None).is_none());
        assert!(registry.resolve("run", "missing", None).is_none());
        assert!(registry.resolve("run", "one", Some("other")).is_none());
        for stage in [None, Some("stage")] {
            let (resolved, sender) = registry.resolve("run", "one", stage).unwrap();
            assert_eq!(resolved, identity("one"));
            let _receipt = sender.try_send("first".into(), "one".into()).unwrap();
            assert_eq!(receive(&mut first).text, "one");
        }
        revoker.revoke();
        assert!(registry.resolve("run", "one", None).is_none());
        let (resolved, sender) = registry.resolve("run", "two", None).unwrap();
        assert_eq!(resolved, identity("two"));
        let _receipt = sender.try_send("second".into(), "two".into()).unwrap();
        assert_eq!(receive(&mut second).text, "two");
    }

    #[test]
    fn resolve_never_arbitrarily_selects_an_ambiguous_stage() {
        let registry = InputRegistry::default();
        let (_first, revoker) = registry.register(identity("one"), |_| {});
        let mut other = identity("one");
        other.stage_name = "other".into();
        let (_second, _revoker) = registry.register(other.clone(), |_| {});
        assert!(registry.resolve("run", "one", None).is_none());
        assert_eq!(
            registry.resolve("run", "one", Some("stage")).unwrap().0,
            identity("one")
        );
        assert_eq!(
            registry.resolve("run", "one", Some("other")).unwrap().0,
            other
        );
        revoker.revoke();
        assert_eq!(registry.resolve("run", "one", None).unwrap().0, other);
    }

    #[tokio::test]
    async fn closed_wakes_all_waiters_and_latches_after_revocation() {
        let registry = InputRegistry::default();
        let (receiver, revoker) = registry.register(identity("one"), |_| {});
        let sender = registry.sender(&identity("one")).unwrap();
        assert!(!sender.is_closed());
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(flag.clone());
        let mut context = Context::from_waker(&waker);
        let first = sender.closed();
        let second = sender.closed();
        tokio::pin!(first, second);
        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(second.as_mut().poll(&mut context).is_pending());
        revoker.revoke();
        assert!(flag.0.load(Ordering::SeqCst));
        assert!(first.as_mut().poll(&mut context).is_ready());
        assert!(second.as_mut().poll(&mut context).is_ready());
        assert!(sender.is_closed());
        sender.closed().await;
        revoker.revoke();
        drop(receiver);
        sender.closed().await;
    }

    #[tokio::test]
    async fn closed_observes_receiver_drop_and_does_not_follow_replacement() {
        let registry = InputRegistry::default();
        let (receiver, _revoker) = registry.register(identity("one"), |_| {});
        let old = registry.sender(&identity("one")).unwrap();
        // Constructing the future must not create a missed-notification window.
        let closed = old.closed();
        drop(receiver);
        closed.await;
        let (_receiver, revoker) = registry.register(identity("one"), |_| {});
        let new = registry.resolve("run", "one", None).unwrap().1;
        assert!(!new.is_closed());
        old.closed().await;
        assert_eq!(
            old.try_send("old".into(), "text".into()).unwrap_err(),
            InputRejected::Revoked
        );
        revoker.revoke();
        new.closed().await;
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn live_duplicate_identity_is_rejected() {
        let registry = InputRegistry::default();
        let (_receiver, _revoker) = registry.register(identity("one"), |_| {});
        registry.register(identity("one"), |_| {});
    }
}
