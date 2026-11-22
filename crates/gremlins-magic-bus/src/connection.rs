//! Connection-scoped RAII guards and a shared socket writer.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::event_kind::EventKind;
use crate::Subscriber;

// ---------------------------------------------------------------------------
// SharedWriter — Arc<Mutex<WriteHalf>> for concurrent socket writes
// ---------------------------------------------------------------------------

pub struct SharedWriter {
    inner: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    close_tx: tokio::sync::watch::Sender<bool>,
}

impl SharedWriter {
    pub fn new(inner: tokio::net::unix::OwnedWriteHalf) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
            close_tx: tokio::sync::watch::channel(false).0,
        }
    }

    /// Latch connection closure for all current and future waiters.
    pub fn mark_closed(&self) {
        self.close_tx.send_replace(true);
    }

    pub async fn closed(&self) {
        let mut rx = self.close_tx.subscribe();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    /// Owned by the socket reader, never by a writer clone or delivery task.
    pub fn read_lifetime_guard(&self) -> ReadLifetimeGuard {
        ReadLifetimeGuard(self.close_tx.clone())
    }

    pub async fn write_json_line(&self, value: &Value) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        let mut bytes = serde_json::to_vec(value).map_err(|e| format!("JSON encode: {e}"))?;
        bytes.push(b'\n');
        let result = guard.write_all(&bytes).await;
        if result.is_err() {
            self.mark_closed();
        }
        result.map_err(|e| format!("write error: {e}"))
    }
}

impl Clone for SharedWriter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            close_tx: self.close_tx.clone(),
        }
    }
}

/// Marks closure when the socket reader exits, including cancellation.
pub struct ReadLifetimeGuard(tokio::sync::watch::Sender<bool>);

impl Drop for ReadLifetimeGuard {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

// ---------------------------------------------------------------------------
// BusSubGuard
// ---------------------------------------------------------------------------

/// RAII guard that unsubscribes a bus subscriber on drop.
///
/// Chat turns and debug sessions register run-scoped subscriptions; a
/// subscription that outlived its consumer would accumulate and — once its
/// bounded buffer filled — stall the agent loop of a later run that reuses the
/// same id. Dropping this guard unsubscribes on every exit path, including
/// early returns.
pub struct BusSubGuard {
    subscriber: Option<Subscriber<EventKind>>,
}

impl BusSubGuard {
    pub fn new(subscriber: Subscriber<EventKind>) -> Self {
        Self {
            subscriber: Some(subscriber),
        }
    }
}

impl Drop for BusSubGuard {
    fn drop(&mut self) {
        if let Some(subscriber) = self.subscriber.take() {
            crate::concrete::init_bus().unsubscribe(&subscriber);
        }
    }
}

// ---------------------------------------------------------------------------
// Connection guard — RAII closure-based counter
// ---------------------------------------------------------------------------

/// RAII guard: calls an "up" closure on construction and a "down" closure on
/// drop.  No longer generic over an actor message type — callers pass
/// arbitrary hooks via closures.
pub struct ConnectionGuard {
    down: Option<Box<dyn FnOnce() + Send>>,
}

impl ConnectionGuard {
    /// `up` is called immediately; `down` is called on drop.
    pub fn new(up: impl FnOnce() + Send + 'static, down: impl FnOnce() + Send + 'static) -> Self {
        up();
        Self {
            down: Some(Box::new(down)),
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(down) = self.down.take() {
            down();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SharedWriter;
    use std::time::Duration;

    #[tokio::test]
    async fn reader_closure_is_latched_across_writer_clones() {
        let (socket, _peer) = tokio::net::UnixStream::pair().unwrap();
        let (_read, write) = socket.into_split();
        let writer = SharedWriter::new(write);
        let clone = writer.clone();
        let guard = writer.read_lifetime_guard();
        assert!(tokio::time::timeout(Duration::ZERO, clone.closed())
            .await
            .is_err());
        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), clone.closed())
            .await
            .unwrap();
        writer.mark_closed();
        tokio::time::timeout(Duration::from_secs(1), writer.closed())
            .await
            .unwrap();
    }
}
