//! Global event bus and connection counter.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use tokio::sync::watch;

pub(crate) use crate::executor::event::EventKind;
use crate::magic_bus::{
    Bus, CorrId, DispatchEvent, ReceiverHalf, Subscriber, SubscriberRegistration,
};

use super::get_run_map;

// ---------------------------------------------------------------------------
// Global event bus
// ---------------------------------------------------------------------------

static BUS: OnceLock<Bus<EventKind>> = OnceLock::new();

pub(crate) fn init_bus() -> &'static Bus<EventKind> {
    BUS.get_or_init(|| {
        let bus = Bus::new();
        // One-time registration: log every LogLine event via the log crate so
        // call sites only need `emit_event(EventKind::LogLine { … })` — the bus
        // subscriber handles the `log::info!()` side automatically.
        //
        // Guard with try_current() so emit_event stays safe to call from
        // synchronous startup code or non-Tokio threads; the subscription is
        // skipped when no runtime handle is available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let (_, mut rx) = bus
                .subscribe(SubscriberRegistration::lag_tolerant(CorrId::new(), 10_000))
                .expect("log subscriber");
            handle.spawn(async move {
                loop {
                    match rx.recv().await {
                        crate::magic_bus::Recv::Event(dispatch) => {
                            if let EventKind::LogLine { line, .. } = dispatch.kind() {
                                log::info!("{line}");
                            }
                        }
                        crate::magic_bus::Recv::Lagged(n) => {
                            log::warn!("log subscriber lagged; {n} log lines dropped");
                        }
                        crate::magic_bus::Recv::Closed => break,
                    }
                }
            });
        }
        bus
    })
}

/// Subscribe to all events on the bus with lag-tolerant delivery (replaces
/// the old broadcast-channel subscribe for the connection forwarder).
pub(crate) fn subscribe_all() -> (Subscriber<EventKind>, ReceiverHalf<EventKind>) {
    init_bus()
        .subscribe(SubscriberRegistration::lag_tolerant(CorrId::new(), 10_000))
        .expect("lag-tolerant subscription cannot fail")
}

/// Subscribe to a single gremlin's run-scoped events with lossless delivery.
pub(crate) fn subscribe_run(id: &str) -> (Subscriber<EventKind>, ReceiverHalf<EventKind>) {
    init_bus()
        .subscribe(SubscriberRegistration::lossless(CorrId::new().with_run(id)))
        .expect("lossless subscription cannot fail")
}

pub(crate) fn emit_event(event: EventKind) {
    // Run-bearing events carry a run-scoped correlation id so per-gremlin
    // subscribers (subscribed with a `r:<id>` prefix) receive only their own
    // run's events; everything else keeps an empty id and reaches all
    // subscribers.
    let corr_id = match &event {
        EventKind::RunStarted { id, .. }
        | EventKind::RunCompleted { id, .. }
        | EventKind::RunFailed { id, .. }
        | EventKind::RunStopped { id }
        | EventKind::RunRemoved { id }
        | EventKind::TokenUsage { id, .. }
        | EventKind::StageTransition { id, .. }
        | EventKind::Bail { id, .. }
        | EventKind::LogLine { id, .. }
        | EventKind::StageBoundary { id, .. } => {
            if id.is_empty() {
                CorrId::new()
            } else {
                CorrId::new().with_run(id.as_str())
            }
        }
        _ => CorrId::new(),
    };
    if let Some(bus) = BUS.get() {
        bus.emit(DispatchEvent::new(corr_id, event));
    }
}

// ---------------------------------------------------------------------------
// Global connection counter
// ---------------------------------------------------------------------------

static CONNECTION_COUNT: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

pub(crate) fn init_connection_counter() -> &'static Arc<AtomicUsize> {
    CONNECTION_COUNT.get_or_init(|| Arc::new(AtomicUsize::new(0)))
}

pub(crate) fn get_connection_count() -> &'static AtomicUsize {
    init_connection_counter().as_ref()
}

/// RAII guard: decrements connection count on drop, signals idle-shutdown.
pub(crate) struct ConnectionGuard {
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl ConnectionGuard {
    pub(crate) fn new(shutdown_tx: &watch::Sender<bool>) -> Self {
        get_connection_count().fetch_add(1, Ordering::Relaxed);
        Self {
            shutdown_tx: Some(shutdown_tx.clone()),
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let prev = get_connection_count().fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            if let Some(tx) = &self.shutdown_tx {
                check_shutdown(tx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shutdown helper
// ---------------------------------------------------------------------------

/// Signal shutdown when there are no running gremlins and no open
/// connections. Safe to call from any task.
pub(crate) fn check_shutdown(shutdown_tx: &watch::Sender<bool>) {
    let runs_empty = get_run_map().lock().unwrap().is_empty();
    let conns_zero = get_connection_count().load(Ordering::Relaxed) == 0;
    if runs_empty && conns_zero {
        let _ = shutdown_tx.send(true);
    }
}
