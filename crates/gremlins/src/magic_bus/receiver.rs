//! The receiver half that a subscriber task polls.

use std::fmt;

use tokio::sync::{broadcast, mpsc};

use super::DispatchEvent;

/// The receiver half that a subscriber task polls.
#[derive(Debug)]
pub enum ReceiverHalf<P> {
    Lossless(mpsc::UnboundedReceiver<DispatchEvent<P>>),
    Backpressured(mpsc::Receiver<DispatchEvent<P>>),
    LagTolerant(broadcast::Receiver<DispatchEvent<P>>),
}

/// The outcome of polling a subscriber receiver.
#[derive(Debug)]
pub enum Recv<P> {
    /// A delivered event.
    Event(DispatchEvent<P>),
    /// The receiver lagged behind and skipped `n` events. Subscribers should
    /// use this to trigger a resync from their own folded state.
    Lagged(u64),
    /// Every sender has been dropped.
    Closed,
}

impl<P: Clone> ReceiverHalf<P> {
    /// Receive the next event.
    ///
    /// Lag-tolerant receivers report [`Recv::Lagged`] when events were dropped
    /// instead of silently skipping them, so the subscriber can resync.
    pub async fn recv(&mut self) -> Recv<P> {
        match self {
            Self::Lossless(rx) => match rx.recv().await {
                Some(event) => Recv::Event(event),
                None => Recv::Closed,
            },
            Self::Backpressured(rx) => match rx.recv().await {
                Some(event) => Recv::Event(event),
                None => Recv::Closed,
            },
            Self::LagTolerant(rx) => match rx.recv().await {
                Ok(event) => Recv::Event(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => Recv::Lagged(skipped),
                Err(broadcast::error::RecvError::Closed) => Recv::Closed,
            },
        }
    }

    /// Synchronous non-blocking receive; `Empty` means no event is currently
    /// buffered. Useful for asserting a subscriber was *not* delivered to.
    pub fn try_recv(&mut self) -> Result<DispatchEvent<P>, TryRecvError> {
        match self {
            Self::Lossless(rx) => rx.try_recv().map_err(|e| match e {
                mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
                mpsc::error::TryRecvError::Disconnected => TryRecvError::Closed,
            }),
            Self::Backpressured(rx) => rx.try_recv().map_err(|e| match e {
                mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
                mpsc::error::TryRecvError::Disconnected => TryRecvError::Closed,
            }),
            Self::LagTolerant(rx) => match rx.try_recv() {
                Ok(event) => Ok(event),
                Err(broadcast::error::TryRecvError::Empty) => Err(TryRecvError::Empty),
                Err(broadcast::error::TryRecvError::Closed) => Err(TryRecvError::Closed),
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    Err(TryRecvError::Lagged(skipped))
                }
            },
        }
    }
}

/// Non-blocking receive outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Closed,
    /// The receiver lagged behind and skipped `n` events.
    Lagged(u64),
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("no event currently buffered"),
            Self::Closed => f.write_str("all senders have been dropped"),
            Self::Lagged(skipped) => write!(f, "receiver lagged and skipped {skipped} events"),
        }
    }
}

impl std::error::Error for TryRecvError {}
