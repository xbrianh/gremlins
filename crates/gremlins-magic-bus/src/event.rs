//! The typed dispatch envelope flowing through the bus.

use std::time::{Instant, SystemTime};

use crate::CorrId;

/// A capture of both monotonic and wall-clock time.
///
/// Monotonic time is what subscribers should use for latency/duration; wall
/// time is what persistence subscribers should stamp onto records.
#[derive(Debug, Clone, Copy)]
pub struct Timestamp {
    monotonic: Instant,
    wall: SystemTime,
}

impl Timestamp {
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    pub fn monotonic(&self) -> Instant {
        self.monotonic
    }

    pub fn wall(&self) -> SystemTime {
        self.wall
    }

    /// Epoch milliseconds for persistence and wire payloads.
    pub fn wall_millis(&self) -> u64 {
        self.wall
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// A single event dispatched across the bus.
///
/// `P` is the domain-specific payload. The bus core does not interpret it; the
/// gremlins layer will instantiate `Bus<EventKind>`.
#[derive(Debug, Clone)]
pub struct DispatchEvent<P> {
    corr_id: CorrId,
    kind: P,
    ts: Timestamp,
    sequence: u64,
}

impl<P> DispatchEvent<P> {
    pub fn new(corr_id: CorrId, kind: P) -> Self {
        Self {
            corr_id,
            kind,
            ts: Timestamp::now(),
            sequence: 0,
        }
    }

    /// Build an event with an explicit timestamp (useful for tests and replay).
    pub fn with_timestamp(corr_id: CorrId, kind: P, ts: Timestamp) -> Self {
        Self {
            corr_id,
            kind,
            ts,
            sequence: 0,
        }
    }

    /// Bus-global emission sequence for a run-scoped event.
    ///
    /// Zero for directly constructed or non-run events. Each run-scoped bus emission
    /// assigns a fresh sequence, shared by live and history envelopes and their clones.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn set_sequence(&mut self, sequence: u64) {
        self.sequence = sequence;
    }

    pub fn corr_id(&self) -> &CorrId {
        &self.corr_id
    }

    pub fn kind(&self) -> &P {
        &self.kind
    }

    /// Consume the event, yielding its payload.
    pub fn into_kind(self) -> P {
        self.kind
    }

    pub fn ts(&self) -> Timestamp {
        self.ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_corr_id_and_payload() {
        let corr_id = CorrId::new().with_run("X").with_turn(1);
        let event = DispatchEvent::new(corr_id.clone(), "payload");

        assert_eq!(event.corr_id(), &corr_id);
        assert_eq!(event.kind(), &"payload");
        assert_eq!(event.sequence(), 0);
        assert_eq!(event.clone().sequence(), 0);
        assert_eq!(
            DispatchEvent::with_timestamp(corr_id, "payload", event.ts()).sequence(),
            0
        );
    }
}
