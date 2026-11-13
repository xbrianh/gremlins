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

/// Subscribe to a single gremlin's run-scoped stream events with lag-tolerant
/// (broadcast) delivery. Drops events on overflow instead of buffering
/// unboundedly — the right choice for debug consumers and the stage-stream
/// forwarder where a slow consumer should not grow daemon memory.
pub(crate) fn subscribe_run_lag_tolerant(
    id: &str,
    capacity: usize,
) -> (Subscriber<EventKind>, ReceiverHalf<EventKind>) {
    init_bus()
        .subscribe(SubscriberRegistration::lag_tolerant(
            CorrId::new().with_run(id),
            capacity,
        ))
        .expect("lag-tolerant subscription cannot fail")
}

/// Subscribe to a single gremlin's run-scoped stream events with bounded,
/// backpressured delivery.
///
/// The chat turn is the sole consumer of a run's stream events. A bounded
/// buffer caps daemon memory for a runaway chat while an awaited send (see
/// [`emit_run_event_async`]) applies backpressure to the agent loop instead
/// of dropping live stream events.
pub(crate) fn subscribe_run_backpressured(
    id: &str,
    bound: usize,
) -> (Subscriber<EventKind>, ReceiverHalf<EventKind>) {
    init_bus()
        .subscribe(SubscriberRegistration::backpressured(
            CorrId::new().with_run(id),
            bound,
        ))
        .expect("backpressured subscription cannot fail")
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
        | EventKind::StageBoundary { id, .. }
        | EventKind::ExecOutput { id, .. } => {
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

/// Emit an event tagged with a specific run id so per-run subscribers
/// (subscribed with an `r:<id>` prefix) receive it.
///
/// Stream events (`Ready`, `StreamChunk`, `ToolResult`, …) carry no `id`
/// field of their own, so [`emit_event`] would route them with an empty
/// correlation id and no per-run subscriber would ever match. The agent-loop
/// stream-event hook knows the run it belongs to and supplies the id here.
///
/// Delivery is asynchronous so a backpressured subscriber (the chat turn)
/// applies backpressure to the agent loop instead of silently dropping live
/// stream events; lossless and lag-tolerant subscribers still deliver
/// synchronously under the hood.
pub(crate) async fn emit_run_event_async(id: &str, event: EventKind) {
    let corr_id = if id.is_empty() {
        CorrId::new()
    } else {
        CorrId::new().with_run(id)
    };
    if let Some(bus) = BUS.get() {
        bus.emit_async(DispatchEvent::new(corr_id, event)).await;
    }
}
