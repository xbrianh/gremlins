//! Magic bus: a domain-agnostic fan-out event registry.
//!
//! The bus knows nothing about gremlin event kinds. It routes
//! [`DispatchEvent<P>`] values to subscribers based on segment-aware
//! [`CorrId`] prefix matching, using the delivery primitive implied by each
//! subscriber's [`DeliveryPolicy`].
//!
//! Registration is rare (gremlin launch / connection open); emission is the
//! hot path, so the registry is a `RwLock<Vec<_>>` with only `try_send`-style
//! work performed under the read lock.

mod corr_id;
mod event;
mod policy;
mod receiver;
mod subscriber;

pub use corr_id::{CorrId, ParseCorrIdError};
pub use event::{DispatchEvent, Timestamp};
pub use policy::{DeliveryPolicy, SubscriberRegistration};
pub use receiver::{ReceiverHalf, Recv, TryRecvError};
pub use subscriber::{Subscriber, SubscriberError};

pub(crate) use subscriber::SubscriberMailbox;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use futures::future::join_all;

/// A fan-out registry keyed by correlation-id prefixes.
pub struct Bus<P> {
    subscribers: RwLock<Vec<(u64, CorrId, SubscriberMailbox<P>)>>,
    next_id: AtomicU64,
}

impl<P: Clone + Send + 'static> Default for Bus<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Clone + Send + 'static> Bus<P> {
    pub fn new() -> Self {
        Self {
            subscribers: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a subscriber and receive its receiver half.
    pub fn subscribe(
        &self,
        registration: SubscriberRegistration,
    ) -> Result<(Subscriber<P>, ReceiverHalf<P>), SubscriberError> {
        let prefix = registration.prefix().clone();
        let policy = registration.policy();
        let (mailbox, receiver) = SubscriberMailbox::for_policy(policy)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let subscriber = Subscriber::new(id, prefix.clone(), mailbox.clone());

        self.subscribers
            .write()
            .expect("subscriber registry poisoned")
            .push((id, prefix, mailbox));

        Ok((subscriber, receiver))
    }

    /// Remove a subscriber by its handle. Idempotent (no-op if already gone).
    ///
    /// After unsubscription the subscriber's receiver will eventually observe
    /// `Closed` once in-flight deliveries drain; the subscriber task can then
    /// close its log blob or connection and exit cleanly.
    pub fn unsubscribe(&self, subscriber: &Subscriber<P>) {
        let id = subscriber.id();
        self.subscribers
            .write()
            .expect("subscriber registry poisoned")
            .retain(|(entry_id, _, _)| *entry_id != id);
    }

    /// Fan out synchronously. Never blocks; backpressured subscribers drop
    /// events when full. This is the path for log-shaped records.
    pub fn emit(&self, event: DispatchEvent<P>) {
        let subscribers = self
            .subscribers
            .read()
            .expect("subscriber registry poisoned");

        for (_id, prefix, mailbox) in subscribers.iter() {
            if event.corr_id().matches_prefix(prefix) {
                mailbox.try_send(event.clone());
            }
        }
    }

    /// Fan out asynchronously, awaiting capacity on backpressured subscribers.
    ///
    /// Matching subscribers are collected under the read lock, then the lock
    /// is released before any `.await`. Delivery to the matching subscribers
    /// happens concurrently, so a slow backpressured subscriber cannot
    /// head-of-line block the others.
    pub async fn emit_async(&self, event: DispatchEvent<P>) {
        let matching: Vec<SubscriberMailbox<P>> = {
            let subscribers = self
                .subscribers
                .read()
                .expect("subscriber registry poisoned");

            subscribers
                .iter()
                .filter(|(_, prefix, _)| event.corr_id().matches_prefix(prefix))
                .map(|(_, _, mailbox)| mailbox.clone())
                .collect()
        };

        join_all(matching.iter().map(|mailbox| mailbox.send(event.clone()))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Payload(u64);

    #[tokio::test]
    async fn emits_only_to_matching_prefixes() {
        let bus: Bus<Payload> = Bus::new();
        let (_, mut rx_a) = bus
            .subscribe(SubscriberRegistration::lossless(
                CorrId::new().with_run("X").with_stage("a"),
            ))
            .expect("lossless subscription cannot fail");
        let (_, mut rx_b) = bus
            .subscribe(SubscriberRegistration::lossless(
                CorrId::new().with_run("X").with_stage("b"),
            ))
            .expect("lossless subscription cannot fail");

        bus.emit(DispatchEvent::new(
            CorrId::new().with_run("X").with_stage("a").with_turn(1),
            Payload(42),
        ));

        match rx_a.recv().await {
            Recv::Event(received) => assert_eq!(received.kind(), &Payload(42)),
            other => panic!("subscriber a should receive an event, got {other:?}"),
        }

        assert!(
            matches!(rx_b.try_recv(), Err(TryRecvError::Empty)),
            "subscriber b must not receive the event"
        );
    }

    #[tokio::test]
    async fn emit_async_delivers_to_lossless_subscribers() {
        let bus: Bus<Payload> = Bus::new();
        let (_, mut rx) = bus
            .subscribe(SubscriberRegistration::lossless(
                CorrId::new().with_run("X"),
            ))
            .expect("lossless subscription cannot fail");

        bus.emit_async(DispatchEvent::new(
            CorrId::new().with_run("X").with_tool("call_0"),
            Payload(7),
        ))
        .await;

        match rx.recv().await {
            Recv::Event(received) => assert_eq!(received.kind(), &Payload(7)),
            other => panic!("subscriber should receive an event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lag_tolerant_receivers_report_lagged_events() {
        let bus: Bus<Payload> = Bus::new();
        let (_, mut rx) = bus
            .subscribe(SubscriberRegistration::lag_tolerant(CorrId::new(), 1))
            .expect("lag-tolerant subscription cannot fail");

        bus.emit(DispatchEvent::new(CorrId::new().with_run("X"), Payload(1)));
        bus.emit(DispatchEvent::new(CorrId::new().with_run("X"), Payload(2)));
        bus.emit(DispatchEvent::new(CorrId::new().with_run("X"), Payload(3)));

        // The first poll reports how many events were skipped, so the
        // subscriber knows to resync; the next poll yields the newest value.
        match rx.recv().await {
            Recv::Lagged(skipped) => assert!(skipped > 0),
            other => panic!("expected a lagged report, got {other:?}"),
        }
        match rx.recv().await {
            Recv::Event(received) => assert_eq!(received.kind().0, 3),
            other => panic!("expected the latest event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sync_emit_drops_when_backpressured_full() {
        let bus: Bus<Payload> = Bus::new();
        let (_, mut rx) = bus
            .subscribe(SubscriberRegistration::backpressured(
                CorrId::new().with_run("X"),
                1,
            ))
            .expect("backpressured subscription cannot fail");

        bus.emit(DispatchEvent::new(CorrId::new().with_run("X"), Payload(1)));
        bus.emit(DispatchEvent::new(CorrId::new().with_run("X"), Payload(2)));

        match rx.recv().await {
            Recv::Event(received) => assert_eq!(received.kind().0, 1),
            other => panic!("expected the first event, got {other:?}"),
        }
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "the full channel must have dropped the second sync emit"
        );
    }

    #[tokio::test]
    async fn emit_async_awaits_backpressure_capacity() {
        let bus: Bus<Payload> = Bus::new();
        let (_, mut rx) = bus
            .subscribe(SubscriberRegistration::backpressured(
                CorrId::new().with_run("X"),
                1,
            ))
            .expect("backpressured subscription cannot fail");

        bus.emit(DispatchEvent::new(CorrId::new().with_run("X"), Payload(1)));

        let bus = std::sync::Arc::new(bus);
        let delivery = {
            let bus = std::sync::Arc::clone(&bus);
            tokio::spawn(async move {
                bus.emit_async(DispatchEvent::new(CorrId::new().with_run("X"), Payload(2)))
                    .await;
            })
        };

        // Give the spawned emit_async a chance to attempt delivery. With the
        // bounded channel full, it must wait for capacity rather than drop.
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().expect("first event is buffered").kind().0, 1);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        // Consuming the first event frees capacity, so emit_async completes.
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("emit_async should deliver once capacity is freed");
        match second {
            Recv::Event(received) => assert_eq!(received.kind().0, 2),
            other => panic!("expected the second event, got {other:?}"),
        }

        delivery.await.expect("spawned emit_async should complete");
    }
}
