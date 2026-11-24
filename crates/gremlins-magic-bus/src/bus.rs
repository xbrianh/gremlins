//! Magic bus: a domain-agnostic fan-out event registry.
//!
//! The bus knows nothing about gremlin event kinds. It routes
//! [`DispatchEvent<P>`] values to subscribers based on segment-aware
//! [`CorrId`] prefix matching, using the delivery primitive implied by each
//! subscriber's [`DeliveryPolicy`].
//!
//! Journal insertion, routing, and history registration share one mutex.
//! No lock is held while running read, persistence, or retention callbacks,
//! or while awaiting bounded channel capacity.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};

use futures::future::join_all;

use crate::subscriber::SubscriberMailbox;
use crate::{
    CorrId, DispatchEvent, ReceiverHalf, Subscriber, SubscriberError, SubscriberRegistration,
};

/// An atomic history/live cut. Events retain their original envelopes and order.
///
/// `complete` is conservatively false after any eviction or retention omission
/// anywhere in this bus, even when it did not match the subscription filter.
#[derive(Clone, Debug)]
pub struct HistorySnapshot<P> {
    pub events: Vec<DispatchEvent<P>>,
    pub complete: bool,
    /// Bus-global sequence of the last run-scoped event at this cut (zero initially).
    pub through_sequence: u64,
    /// Highest sequence evicted or omitted anywhere in the bus (zero if none).
    pub evicted_through_sequence: u64,
}

/// A fan-out registry keyed by correlation-id prefixes.
pub struct Bus<P> {
    state: Mutex<BusState<P>>,
    next_id: AtomicU64,
    retain: Box<dyn Fn(&P) -> bool + Send + Sync>,
}

struct BusState<P> {
    subscribers: Vec<(u64, CorrId, SubscriberMailbox<P>)>,
    journal: BTreeMap<u64, DispatchEvent<P>>,
    runs: HashMap<String, VecDeque<u64>>,
    per_run_limit: usize,
    global_limit: usize,
    sequence: u64,
    evicted_through_sequence: u64,
}

impl<P: Clone> BusState<P> {
    fn record(&mut self, event: &mut DispatchEvent<P>, retain: bool) {
        event.set_sequence(0);
        if event.corr_id().run().is_none() {
            return;
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("bus sequence exhausted");
        let sequence = self.sequence;
        event.set_sequence(sequence);
        let run = event.corr_id().run().expect("run-scoped event");
        if self.per_run_limit == 0 || self.global_limit == 0 || !retain {
            self.evicted_through_sequence = sequence;
            return;
        }
        if let Some(oldest) = self
            .runs
            .get(run)
            .filter(|entries| entries.len() >= self.per_run_limit)
            .and_then(|entries| entries.front())
            .copied()
        {
            self.remove(oldest);
        }
        if self.journal.len() >= self.global_limit {
            let oldest = *self.journal.first_key_value().expect("nonempty journal").0;
            self.remove(oldest);
        }
        self.journal.insert(sequence, event.clone());
        self.runs
            .entry(run.to_owned())
            .or_default()
            .push_back(sequence);
    }

    fn remove(&mut self, sequence: u64) {
        let event = self.journal.remove(&sequence).expect("retained event");
        let run = event.corr_id().run().expect("run-scoped journal");
        let entries = self.runs.get_mut(run).expect("retained run");
        assert_eq!(entries.pop_front(), Some(sequence));
        if entries.is_empty() {
            self.runs.remove(run);
        }
        self.evicted_through_sequence = self.evicted_through_sequence.max(sequence);
    }
}

impl<P: Clone + Send + 'static> Default for Bus<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Clone + Send + 'static> Bus<P> {
    /// Retain up to 16,384 events per exact run and 65,536 globally.
    pub fn new() -> Self {
        Self::with_history_limits(16_384, 65_536)
    }

    /// Construct with custom record bounds (including zero), useful for tests.
    pub fn with_history_limits(per_run_limit: usize, global_limit: usize) -> Self {
        Self::with_history_limits_and_retention(per_run_limit, global_limit, |_| true)
    }

    /// Customize history retention without changing live delivery. Rejected
    /// payloads are never cloned into the journal and make history incomplete.
    /// The predicate runs before acquiring the state mutex and may re-enter the bus.
    pub fn with_history_limits_and_retention(
        per_run_limit: usize,
        global_limit: usize,
        retain: impl Fn(&P) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: Mutex::new(BusState {
                subscribers: Vec::new(),
                journal: BTreeMap::new(),
                runs: HashMap::new(),
                per_run_limit,
                global_limit,
                sequence: 0,
                evicted_through_sequence: 0,
            }),
            next_id: AtomicU64::new(1),
            retain: Box::new(retain),
        }
    }

    /// Run a synchronous read without holding the bus mutex.
    ///
    /// Returns zero as a conservative compatibility watermark: no persisted
    /// replay cut is asserted, even if events were emitted before the read.
    /// The read may re-enter the bus or wait for emissions. Persistence and
    /// publication are not synchronized; callers must not suppress positive
    /// live sequences on the basis of this watermark.
    pub fn with_sequence_snapshot<T>(&self, read: impl FnOnce() -> T) -> (T, u64) {
        (read(), 0)
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

        self.state
            .lock()
            .expect("bus state poisoned")
            .subscribers
            .push((id, prefix, mailbox));

        Ok((subscriber, receiver))
    }

    /// Atomically snapshot matching run-scoped history and register live delivery.
    ///
    /// History retains bounded original envelopes in emission order, without
    /// disk persistence. Non-run events are live only. History is returned
    /// separately, not replayed through the mailbox. See [`HistorySnapshot`]
    /// for conservative completeness and sequence watermark semantics.
    #[allow(clippy::type_complexity)] // Keep the history tuple parallel to subscribe's API.
    pub fn subscribe_with_history(
        &self,
        registration: SubscriberRegistration,
    ) -> Result<(Subscriber<P>, ReceiverHalf<P>, HistorySnapshot<P>), SubscriberError> {
        let prefix = registration.prefix().clone();
        let (mailbox, receiver) = SubscriberMailbox::for_policy(registration.policy())?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let subscriber = Subscriber::new(id, prefix.clone(), mailbox.clone());
        let mut state = self.state.lock().expect("bus state poisoned");
        let history = state
            .journal
            .values()
            .filter(|event| event.corr_id().matches_prefix(&prefix))
            .cloned()
            .collect();
        state.subscribers.push((id, prefix, mailbox));
        Ok((
            subscriber,
            receiver,
            HistorySnapshot {
                events: history,
                complete: state.evicted_through_sequence == 0,
                through_sequence: state.sequence,
                evicted_through_sequence: state.evicted_through_sequence,
            },
        ))
    }

    /// Remove a subscriber by its handle. Idempotent (no-op if already gone).
    ///
    /// After unsubscription the subscriber's receiver will eventually observe
    /// `Closed` once in-flight deliveries drain; the subscriber task can then
    /// close its log blob or connection and exit cleanly.
    pub fn unsubscribe(&self, subscriber: &Subscriber<P>) {
        let id = subscriber.id();
        self.state
            .lock()
            .expect("bus state poisoned")
            .subscribers
            .retain(|(entry_id, _, _)| *entry_id != id);
    }

    /// Fan out synchronously. Never blocks; backpressured subscribers drop
    /// events when full. This is the path for log-shaped records.
    pub fn emit(&self, event: DispatchEvent<P>) {
        self.emit_recorded(event);
    }

    /// Compatibility helper: publish, then pass the assigned envelope to `persist`.
    ///
    /// The callback runs without the bus mutex and may re-enter the bus. There
    /// is no atomic persistence/publication guarantee: subscribers may observe
    /// the event before persistence, and concurrent callbacks may finish out of
    /// sequence order. A callback panic does not undo publication.
    pub fn emit_with_persistence(
        &self,
        event: DispatchEvent<P>,
        persist: impl FnOnce(&DispatchEvent<P>),
    ) {
        let event = self.emit_recorded(event);
        persist(&event);
    }

    fn emit_recorded(&self, mut event: DispatchEvent<P>) -> DispatchEvent<P> {
        let retain = event.corr_id().run().is_some() && (self.retain)(event.kind());
        let mut state = self.state.lock().expect("bus state poisoned");
        state.record(&mut event, retain);
        for (_id, prefix, mailbox) in &state.subscribers {
            if event.corr_id().matches_prefix(prefix) {
                mailbox.try_send(event.clone());
            }
        }
        event
    }

    /// Fan out asynchronously, awaiting capacity on backpressured subscribers.
    ///
    /// Nonbackpressured delivery happens under the journal mutex. Bounded
    /// capacity reservations are first polled under that same mutex to enter
    /// Tokio's FIFO waiter queue in journal order, then awaited concurrently.
    /// FIFO reservations prevent capacity-wait overtaking, but concurrently
    /// resumed tasks can send out of order when multiple permits are available.
    /// Cancellation can drop pending bounded deliveries (not journal entries).
    /// Direct `Subscriber::send` calls bypass this bus ordering contract.
    pub async fn emit_async(&self, mut event: DispatchEvent<P>) {
        let retain = event.corr_id().run().is_some() && (self.retain)(event.kind());
        let pending = {
            let mut state = self.state.lock().expect("bus state poisoned");
            state.record(&mut event, retain);
            let mut pending = Vec::new();
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            for (_, prefix, mailbox) in &state.subscribers {
                if !event.corr_id().matches_prefix(prefix) {
                    continue;
                }
                if let SubscriberMailbox::Backpressured(tx) = mailbox {
                    let mut reservation = Box::pin(tx.clone().reserve_owned());
                    match reservation.as_mut().poll(&mut cx) {
                        Poll::Ready(Ok(permit)) => {
                            permit.send(event.clone());
                        }
                        Poll::Ready(Err(_)) => {}
                        Poll::Pending => pending.push(reservation),
                    }
                } else {
                    mailbox.try_send(event.clone());
                }
            }
            pending
        };
        join_all(pending.into_iter().map(|reservation| {
            let event = event.clone();
            async move {
                if let Ok(permit) = reservation.await {
                    permit.send(event);
                }
            }
        }))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Recv, TryRecvError};

    #[derive(Clone, Debug, PartialEq)]
    struct Payload(u64);

    fn assert_same(actual: &DispatchEvent<Payload>, expected: &DispatchEvent<Payload>) {
        assert_eq!(actual.corr_id(), expected.corr_id());
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.ts().monotonic(), expected.ts().monotonic());
        assert_eq!(actual.ts().wall(), expected.ts().wall());
    }

    #[test]
    fn persistence_callback_can_reenter_after_publication() {
        let bus = Bus::new();
        let id = CorrId::new().with_run("persist");
        let (_, mut rx) = bus
            .subscribe(SubscriberRegistration::lossless(id.clone()))
            .unwrap();
        let mut persisted = Vec::new();
        for number in 1..=3 {
            bus.emit_with_persistence(DispatchEvent::new(id.clone(), Payload(number)), |event| {
                persisted.push(event.sequence());
                assert_eq!(event.sequence(), number);
                assert!(bus.state.try_lock().is_ok());
                let history = snapshot(&bus, Some("persist"));
                assert_eq!(history.through_sequence, number);
                assert_eq!(rx.try_recv().unwrap().sequence(), number);
                // Reentrant publication must not wait on the callback's lock.
                bus.emit(DispatchEvent::new(CorrId::new(), Payload(99)));
                assert_eq!(bus.with_sequence_snapshot(|| "read").1, 0);
            });
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        }
        let (covered, publication) = bus.with_sequence_snapshot(|| persisted.clone());
        assert_eq!(covered, vec![1, 2, 3]);
        assert_eq!(publication, 0);
        bus.emit(DispatchEvent::new(id, Payload(4)));
        assert_eq!(rx.try_recv().unwrap().sequence(), 4);
        assert_eq!(covered.last(), Some(&3));
    }

    #[test]
    fn sequence_snapshot_allows_reentrant_and_concurrent_emission() {
        let bus = Bus::new();
        let id = CorrId::new().with_run("cut");
        bus.emit(DispatchEvent::new(id.clone(), Payload(1)));
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let (value, sequence) = bus.with_sequence_snapshot(|| {
                scope.spawn(|| {
                    bus.emit(DispatchEvent::new(id, Payload(2)));
                    done_tx.send(()).unwrap();
                });
                assert!(bus.state.try_lock().is_ok());
                record(&bus, "cut", 3);
                let history = snapshot(&bus, Some("cut"));
                assert!(history.through_sequence >= 2);
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("read must not block concurrent publication");
                "disk tail"
            });
            assert_eq!((value, sequence), ("disk tail", 0));
        });
        assert_eq!(snapshot(&bus, Some("cut")).through_sequence, 3);
        assert_eq!(bus.with_sequence_snapshot(|| ()).1, 0);
    }

    #[tokio::test]
    async fn retention_callback_can_reenter_sync_and_async_emission() {
        use std::sync::{Arc, OnceLock, Weak};

        let slot = Arc::new(OnceLock::<Weak<Bus<Payload>>>::new());
        let callback_slot = slot.clone();
        let bus = Arc::new(Bus::with_history_limits_and_retention(8, 8, move |_| {
            let bus = callback_slot.get().unwrap().upgrade().unwrap();
            assert!(bus.state.try_lock().is_ok());
            let _ = snapshot(&bus, Some("retention"));
            // Non-run emissions do not invoke the retention predicate.
            bus.emit(DispatchEvent::new(CorrId::new(), Payload(99)));
            true
        }));
        slot.set(Arc::downgrade(&bus)).unwrap();
        let (_, mut rx) = bus
            .subscribe(SubscriberRegistration::lossless(CorrId::new()))
            .unwrap();
        record(&bus, "retention", 1);
        bus.emit_async(DispatchEvent::new(
            CorrId::new().with_run("retention"),
            Payload(2),
        ))
        .await;
        for (value, sequence) in [(99, 0), (1, 1), (99, 0), (2, 2)] {
            let event = rx.try_recv().unwrap();
            assert_eq!(event.kind().0, value);
            assert_eq!(event.sequence(), sequence);
        }
        let history = snapshot(&bus, Some("retention"));
        assert!(history.complete);
        assert_eq!(history.events.len(), 2);
    }

    #[tokio::test]
    async fn snapshot_and_future_preserve_envelopes_and_filter_scope() {
        for lag_tolerant in [false, true] {
            let bus = Bus::new();
            let base = CorrId::new()
                .with_run("parent")
                .with_stage("build")
                .with_execution_id("exec");
            let ids = [
                base.clone(),
                CorrId::new()
                    .with_run("parent:child")
                    .with_stage("build")
                    .with_execution_id("exec"),
                base.clone().with_run("parentish"),
                base.clone().with_stage("test"),
                base.clone().with_execution_id("exec:child"),
                CorrId::new(),
            ];
            let events: Vec<_> = ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| DispatchEvent::new(id, Payload(i as u64)))
                .collect();
            for (i, event) in events.iter().enumerate() {
                if i % 2 == 0 {
                    bus.emit(event.clone());
                } else {
                    bus.emit_async(event.clone()).await;
                }
            }
            let registration = if lag_tolerant {
                SubscriberRegistration::lag_tolerant(base, 32)
            } else {
                SubscriberRegistration::lossless(base)
            };
            let (_, mut rx, history) = bus.subscribe_with_history(registration.clone()).unwrap();
            assert!(history.complete);
            assert_eq!(history.through_sequence, 5);
            assert_eq!(history.events.len(), 2);
            for (actual, expected) in history.events.iter().zip(&events) {
                assert_same(actual, expected);
            }
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            for event in &events {
                bus.emit_async(event.clone()).await;
            }
            let mut combined = history.events;
            for expected in &events[..2] {
                let actual = rx.try_recv().unwrap();
                assert_same(&actual, expected);
                combined.push(actual);
            }
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            let (_, _, snapshot) = bus.subscribe_with_history(registration).unwrap();
            assert_eq!(snapshot.events.len(), combined.len());
            for (actual, expected) in snapshot.events.iter().zip(&combined) {
                assert_same(actual, expected);
            }
            let (_, _, all) = bus
                .subscribe_with_history(SubscriberRegistration::lossless(CorrId::new()))
                .unwrap();
            assert_eq!(all.events.len(), 10, "non-run events are not journaled");
        }
    }

    #[tokio::test]
    async fn pending_bounded_send_does_not_delay_live_or_duplicate_snapshot() {
        for lag_tolerant in [false, true] {
            let bus = Bus::new();
            let id = CorrId::new().with_run("X");
            let (_, mut bounded) = bus
                .subscribe(SubscriberRegistration::backpressured(id.clone(), 1))
                .unwrap();
            let registration = if lag_tolerant {
                SubscriberRegistration::lag_tolerant(id.clone(), 32)
            } else {
                SubscriberRegistration::lossless(id.clone())
            };
            let (_, mut live, _) = bus.subscribe_with_history(registration.clone()).unwrap();
            let events: Vec<_> = (0..4)
                .map(|i| DispatchEvent::new(id.clone(), Payload(i)))
                .collect();
            bus.emit(events[0].clone());
            let mut pending = Box::pin(bus.emit_async(events[1].clone()));
            assert!(matches!(futures::poll!(&mut pending), Poll::Pending));
            let (_, mut future, history) =
                bus.subscribe_with_history(registration.clone()).unwrap();
            assert_eq!(history.through_sequence, 2);
            assert_eq!(history.events.len(), 2);
            assert_eq!(history.events[1].sequence(), 2);
            let mut history = history.events;
            bus.emit(events[2].clone());
            let mut later = Box::pin(bus.emit_async(events[3].clone()));
            assert!(matches!(futures::poll!(&mut later), Poll::Pending));
            for expected in &events {
                assert_same(&live.try_recv().unwrap(), expected);
            }
            for expected in &events[2..] {
                let actual = future.try_recv().unwrap();
                assert_same(&actual, expected);
                history.push(actual);
            }
            assert!(matches!(future.try_recv(), Err(TryRecvError::Empty)));
            assert_same(&bounded.try_recv().unwrap(), &events[0]);
            pending.await;
            let delivered = bounded.try_recv().unwrap();
            assert_same(&delivered, &events[1]);
            assert_eq!(delivered.sequence(), 2);
            later.await;
            assert_same(&bounded.try_recv().unwrap(), &events[3]);
            let (_, _, snapshot) = bus.subscribe_with_history(registration).unwrap();
            assert_eq!(snapshot.events.len(), history.len());
            for (actual, expected) in snapshot.events.iter().zip(&history) {
                assert_same(actual, expected);
            }
        }
    }

    fn snapshot(bus: &Bus<Payload>, run: Option<&str>) -> HistorySnapshot<Payload> {
        let prefix = run.map_or_else(CorrId::new, |run| CorrId::new().with_run(run));
        let (sub, _, history) = bus
            .subscribe_with_history(SubscriberRegistration::lossless(prefix))
            .unwrap();
        bus.unsubscribe(&sub);
        history
    }

    fn record(bus: &Bus<Payload>, run: &str, value: u64) {
        bus.emit(DispatchEvent::new(
            CorrId::new().with_run(run),
            Payload(value),
        ));
    }

    #[test]
    fn per_run_eviction_is_exact_and_preserves_global_order() {
        let bus = Bus::with_history_limits(2, 10);
        record(&bus, "parent", 1);
        record(&bus, "parent:child", 2);
        record(&bus, "parent", 3);
        record(&bus, "parent", 4);
        let history = snapshot(&bus, Some("parent"));
        assert_eq!(
            history
                .events
                .iter()
                .map(|e| e.kind().0)
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );
        assert!(!history.complete);
        assert_eq!(history.through_sequence, 4);
        assert_eq!(history.evicted_through_sequence, 1);
        assert_eq!(bus.state.lock().unwrap().runs.len(), 2);
        assert!(!snapshot(&bus, Some("unrelated")).complete);
    }

    #[test]
    fn global_eviction_drops_oldest_and_bounds_metadata_across_many_runs() {
        let bus = Bus::with_history_limits(2, 3);
        for i in 0..1000 {
            record(&bus, &format!("run-{i}"), i);
            let state = bus.state.lock().unwrap();
            assert!(state.journal.len() <= 3);
            assert!(state.runs.len() <= 3);
            assert_eq!(
                state.runs.values().map(VecDeque::len).sum::<usize>(),
                state.journal.len()
            );
        }
        let history = snapshot(&bus, None);
        assert_eq!(
            history
                .events
                .iter()
                .map(|e| e.kind().0)
                .collect::<Vec<_>>(),
            [997, 998, 999]
        );
        assert!(!history.complete);
        assert_eq!(history.through_sequence, 1000);
        assert_eq!(history.evicted_through_sequence, 997);
        assert!(snapshot(&bus, Some("run-0")).events.is_empty());
    }

    #[tokio::test]
    async fn omissions_and_zero_limits_keep_no_run_metadata_but_deliver_live() {
        for bus in [
            Bus::with_history_limits(0, 3),
            Bus::with_history_limits(3, 0),
            Bus::with_history_limits_and_retention(3, 3, |_| false),
        ] {
            let (_, mut rx) = bus
                .subscribe(SubscriberRegistration::lossless(CorrId::new()))
                .unwrap();
            for i in 0..100 {
                bus.emit_async(DispatchEvent::new(
                    CorrId::new().with_run(format!("run-{i}")),
                    Payload(i),
                ))
                .await;
                let live = rx.try_recv().unwrap();
                assert_eq!(live.kind().0, i);
                assert_eq!(live.sequence(), i + 1);
            }
            bus.emit(DispatchEvent::new(CorrId::new(), Payload(101)));
            assert_eq!(rx.try_recv().unwrap().sequence(), 0);
            bus.emit_async(DispatchEvent::new(CorrId::new(), Payload(102)))
                .await;
            assert_eq!(rx.try_recv().unwrap().sequence(), 0);
            let history = snapshot(&bus, None);
            assert!(history.events.is_empty());
            assert!(!history.complete);
            assert_eq!(history.through_sequence, 100);
            assert_eq!(history.evicted_through_sequence, 100);
            assert!(bus.state.lock().unwrap().runs.is_empty());
        }
    }

    #[tokio::test]
    async fn emission_sequences_match_live_history_and_survive_cloning() {
        let bus = Bus::new();
        let (_, mut lossless) = bus
            .subscribe(SubscriberRegistration::lossless(CorrId::new()))
            .unwrap();
        let (_, mut broadcast) = bus
            .subscribe(SubscriberRegistration::lag_tolerant(CorrId::new(), 8))
            .unwrap();
        let (_, mut bounded) = bus
            .subscribe(SubscriberRegistration::backpressured(CorrId::new(), 8))
            .unwrap();
        let original = DispatchEvent::new(CorrId::new().with_run("X"), Payload(1));
        bus.emit(original.clone());
        bus.emit_async(original.clone()).await;
        let (_, mut rx, history) = bus
            .subscribe_with_history(SubscriberRegistration::lossless(CorrId::new()))
            .unwrap();
        assert_eq!(history.events.len(), 2);
        assert_eq!(history.through_sequence, 2);
        for (index, retained) in history.events.iter().enumerate() {
            assert_eq!(retained.sequence(), index as u64 + 1);
            assert_same(retained, &original);
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        let stamped = history.events[0].clone();
        assert_eq!(stamped.sequence(), 1);
        bus.emit_async(stamped.clone()).await;
        assert_eq!(stamped.sequence(), 1);
        assert_eq!(original.sequence(), 0);
        let live = rx.try_recv().unwrap();
        assert_eq!(live.sequence(), 3);
        assert_same(&live, &original);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        let later = snapshot(&bus, None);
        assert_eq!(later.events.len(), 3);
        assert_eq!(later.through_sequence, 3);
        for (index, retained) in later.events.iter().enumerate() {
            let sequence = index as u64 + 1;
            assert_eq!(retained.sequence(), sequence);
            assert_eq!(retained.clone().sequence(), sequence);
            assert_same(retained, &original);
            for delivered in [
                lossless.try_recv().unwrap(),
                broadcast.try_recv().unwrap(),
                bounded.try_recv().unwrap(),
            ] {
                assert_eq!(delivered.sequence(), sequence);
                assert_same(&delivered, retained);
            }
        }
    }

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
    async fn execution_filters_route_exactly_without_changing_run_filters() {
        for asynchronous in [false, true] {
            let bus: Bus<Payload> = Bus::new();
            let prefixes = [
                CorrId::new(),
                CorrId::new().with_run("parent"),
                CorrId::new().with_execution_id("exec"),
                CorrId::new().with_run("parent").with_execution_id("exec"),
                CorrId::new().with_run("other").with_execution_id("exec"),
            ];
            let mut receivers: Vec<_> = prefixes
                .into_iter()
                .map(|prefix| {
                    bus.subscribe(SubscriberRegistration::lossless(prefix))
                        .unwrap()
                        .1
                })
                .collect();
            let base = CorrId::new().with_run("parent:child").with_stage("build");
            let events = [
                base.clone().with_execution_id("exec"),
                base.clone().with_execution_id("exec:child"),
                base.clone().with_execution_id("execution"),
                base.clone().with_execution_id("other"),
                base,
            ];
            for (index, corr_id) in events.into_iter().enumerate() {
                let event = DispatchEvent::new(corr_id, Payload(index as u64));
                if asynchronous {
                    bus.emit_async(event).await;
                } else {
                    bus.emit(event);
                }
            }
            let expected: &[&[u64]] = &[&[0, 1, 2, 3, 4], &[0, 1, 2, 3, 4], &[0], &[0], &[]];
            for (rx, expected) in receivers.iter_mut().zip(expected) {
                for value in *expected {
                    assert_eq!(rx.try_recv().unwrap().kind(), &Payload(*value));
                }
                assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            }
        }
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
