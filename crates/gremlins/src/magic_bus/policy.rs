//! Delivery semantics and subscriber registration.

use super::CorrId;

/// How a subscriber wants events delivered.
///
/// The three variants map directly onto the channel primitives the system
/// already relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryPolicy {
    /// Unbounded `mpsc` — producers never block. Equivalent to today's
    /// `log_tx`.
    Lossless,
    /// Bounded `mpsc` — producers await capacity on `emit_async`.
    /// Used by the chat turn's stream subscriber.
    Backpressured { bound: usize },
    /// `broadcast` — drops on `Lagged`; the subscriber resyncs from its own
    /// folded state. Equivalent to today's `EVENT_BROADCAST`.
    LagTolerant { capacity: usize },
}

/// A prefix plus the delivery policy its subscriber wants.
#[derive(Debug, Clone)]
pub struct SubscriberRegistration {
    prefix: CorrId,
    policy: DeliveryPolicy,
}

impl SubscriberRegistration {
    pub fn new(prefix: CorrId, policy: DeliveryPolicy) -> Self {
        Self { prefix, policy }
    }

    pub fn lossless(prefix: CorrId) -> Self {
        Self::new(prefix, DeliveryPolicy::Lossless)
    }

    pub fn backpressured(prefix: CorrId, bound: usize) -> Self {
        Self::new(prefix, DeliveryPolicy::Backpressured { bound })
    }

    pub fn lag_tolerant(prefix: CorrId, capacity: usize) -> Self {
        Self::new(prefix, DeliveryPolicy::LagTolerant { capacity })
    }

    pub fn prefix(&self) -> &CorrId {
        &self.prefix
    }

    pub fn policy(&self) -> DeliveryPolicy {
        self.policy
    }
}
