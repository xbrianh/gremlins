//! Execution-scoped correction inboxes, independent of debugger attachments.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{mpsc, oneshot};

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
    /// Panics if this identity already has a live, unrevoked inbox.
    pub(crate) fn register(
        &self,
        identity: ExecutionIdentity,
        initialize: impl FnOnce(Arc<InputRevoker>),
    ) -> (InputReceiver, Arc<InputRevoker>) {
        let (sender, receiver) = mpsc::channel(INPUT_CAPACITY);
        let shared = Arc::new(Shared {
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
        if let Some(existing) = entries.get(&identity).and_then(Weak::upgrade) {
            assert!(
                existing.state.lock().unwrap().sender.is_none(),
                "execution input identity is already registered: {identity:?}"
            );
        }
        entries.insert(identity, Arc::downgrade(&shared));
        (
            InputReceiver {
                shared: shared.clone(),
                receiver,
            },
            revoker,
        )
    }

    pub(crate) fn sender(&self, identity: &ExecutionIdentity) -> Option<InputSender> {
        let shared = self.entries.lock().unwrap().get(identity)?.upgrade()?;
        let active = shared.state.lock().unwrap().sender.is_some();
        active.then_some(InputSender { shared })
    }
}

#[derive(Debug)]
struct Shared {
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
    text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct InputSender {
    shared: Arc<Shared>,
}

impl InputSender {
    /// Acceptance means enqueued, not yet incorporated into the conversation.
    pub(crate) fn try_send(
        &self,
        request_id: String,
        text: String,
    ) -> Result<Receipt, InputRejected> {
        let mut state = self.shared.state.lock().unwrap();
        let sender = state.sender.as_ref().ok_or(InputRejected::Revoked)?;
        let id = state.next_id;
        let next_id = id.checked_add(1).expect("execution input id exhausted");
        sender
            .try_send(Queued {
                id,
                request_id,
                text,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InputRejected::Full,
                mpsc::error::TrySendError::Closed(_) => InputRejected::Revoked,
            })?;
        let (completion, receipt) = oneshot::channel();
        state.next_id = next_id;
        state.pending.insert(id, completion);
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
    pub(crate) fn try_recv(&mut self) -> Result<Correction, mpsc::error::TryRecvError> {
        let state = self.shared.state.lock().unwrap();
        if state.sender.is_none() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        drop(state);
        let queued = self.receiver.try_recv()?;
        let state = self.shared.state.lock().unwrap();
        if state.sender.is_none() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        Ok(Correction {
            request_id: queued.request_id,
            text: queued.text,
            id: queued.id,
            shared: self.shared.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(stage_run_id: &str) -> ExecutionIdentity {
        ExecutionIdentity {
            run_id: "run".into(),
            stage_name: "stage".into(),
            stage_run_id: stage_run_id.into(),
        }
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
        let correction = receiver.try_recv().unwrap();
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
        let correction = receiver.try_recv().unwrap();
        receiver.try_recv().unwrap().incorporate(|_, _| {});
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
            let correction = receiver.try_recv().unwrap();
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
        receiver.try_recv().unwrap().incorporate(|_, _| {});
        let mut in_flight = sender.try_send("polled".into(), "text".into()).unwrap();
        let correction = receiver.try_recv().unwrap();
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
        let correction = receiver.try_recv().unwrap();
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
        let correction = receiver.try_recv().unwrap();
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
    #[should_panic(expected = "already registered")]
    fn live_duplicate_identity_is_rejected() {
        let registry = InputRegistry::default();
        let (_receiver, _revoker) = registry.register(identity("one"), |_| {});
        registry.register(identity("one"), |_| {});
    }
}
