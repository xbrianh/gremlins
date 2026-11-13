//! Global event bus.

use std::sync::OnceLock;

pub(crate) use crate::executor::event::EventKind;
use crate::magic_bus::{
    Bus, CorrId, DispatchEvent, ReceiverHalf, Subscriber, SubscriberRegistration,
};

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
