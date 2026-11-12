//! Producer-side handles and the mailbox that delivers to a receiver.

use std::fmt;

use tokio::sync::{broadcast, mpsc};

use super::receiver::ReceiverHalf;
use super::{CorrId, DeliveryPolicy, DispatchEvent};

/// The concrete channel backing a subscriber's delivery policy.
///
/// This is an implementation detail: consumers interact with the bus through
/// [`DeliveryPolicy`] and [`super::SubscriberRegistration`], and receive a
/// [`super::ReceiverHalf`].
#[derive(Clone)]
pub(crate) enum SubscriberMailbox<P> {
    Lossless(mpsc::UnboundedSender<DispatchEvent<P>>),
    Backpressured(mpsc::Sender<DispatchEvent<P>>),
    LagTolerant(broadcast::Sender<DispatchEvent<P>>),
}

impl<P> fmt::Debug for SubscriberMailbox<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lossless(_) => f.write_str("Lossless"),
            Self::Backpressured(_) => f.write_str("Backpressured"),
            Self::LagTolerant(_) => f.write_str("LagTolerant"),
        }
    }
}

/// Errors raised while creating a subscriber mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriberError {
    /// Bounded and lag-tolerant channels must have a positive capacity.
    ZeroCapacity,
}

impl fmt::Display for SubscriberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => f.write_str("channel capacity must be greater than zero"),
        }
    }
}

impl std::error::Error for SubscriberError {}

impl<P: Clone> SubscriberMailbox<P> {
    /// Create a mailbox (and matching receiver) for a delivery policy.
    pub(crate) fn for_policy(
        policy: DeliveryPolicy,
    ) -> Result<(Self, ReceiverHalf<P>), SubscriberError> {
        match policy {
            DeliveryPolicy::Lossless => {
                let (tx, rx) = mpsc::unbounded_channel();
                Ok((Self::Lossless(tx), ReceiverHalf::Lossless(rx)))
            }
            DeliveryPolicy::Backpressured { bound } => {
                if bound == 0 {
                    return Err(SubscriberError::ZeroCapacity);
                }
                let (tx, rx) = mpsc::channel(bound);
                Ok((Self::Backpressured(tx), ReceiverHalf::Backpressured(rx)))
            }
            DeliveryPolicy::LagTolerant { capacity } => {
                if capacity == 0 {
                    return Err(SubscriberError::ZeroCapacity);
                }
                let (tx, rx) = broadcast::channel(capacity);
                Ok((Self::LagTolerant(tx), ReceiverHalf::LagTolerant(rx)))
            }
        }
    }

    /// Synchronous send path used by [`super::Bus::emit`].
    ///
    /// Lossless pushes onto an unbounded channel. Backpressured performs a
    /// `try_send` and drops the event when full. LagTolerant broadcasts and
    /// ignores `Lagged` errors (the receiver learns about the drop on its next
    /// poll).
    pub(crate) fn try_send(&self, event: DispatchEvent<P>) {
        match self {
            Self::Lossless(tx) => {
                let _ = tx.send(event);
            }
            Self::Backpressured(tx) => {
                let _ = tx.try_send(event);
            }
            Self::LagTolerant(tx) => {
                let _ = tx.send(event);
            }
        }
    }

    /// Await capacity for backpressured subscribers; otherwise delegate to
    /// [`Self::try_send`].
    pub(crate) async fn send(&self, event: DispatchEvent<P>) {
        match self {
            Self::Backpressured(tx) => {
                let _ = tx.send(event).await;
            }
            _ => self.try_send(event),
        }
    }
}

/// The producer-side handle returned by `Bus::subscribe()`.
#[derive(Debug, Clone)]
pub struct Subscriber<P> {
    prefix: CorrId,
    mailbox: SubscriberMailbox<P>,
}

impl<P: Clone> Subscriber<P> {
    pub(crate) fn new(prefix: CorrId, mailbox: SubscriberMailbox<P>) -> Self {
        Self { prefix, mailbox }
    }

    pub fn prefix(&self) -> &CorrId {
        &self.prefix
    }

    /// Synchronous send. See [`SubscriberMailbox::try_send`].
    ///
    /// `Bus::emit` performs the prefix check itself; this exists for tests and
    /// for producers that already know the match.
    pub fn send(&self, event: DispatchEvent<P>) {
        self.mailbox.try_send(event);
    }

    /// Async send that awaits capacity for backpressured delivery.
    pub async fn send_async(&self, event: DispatchEvent<P>) {
        self.mailbox.send(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct Payload;

    #[test]
    fn rejects_zero_capacity_channels() {
        assert_eq!(
            SubscriberMailbox::<Payload>::for_policy(DeliveryPolicy::Backpressured { bound: 0 })
                .unwrap_err(),
            SubscriberError::ZeroCapacity
        );
        assert_eq!(
            SubscriberMailbox::<Payload>::for_policy(DeliveryPolicy::LagTolerant { capacity: 0 })
                .unwrap_err(),
            SubscriberError::ZeroCapacity
        );
    }
}
