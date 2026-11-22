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
}

impl SharedWriter {
    pub fn new(inner: tokio::net::unix::OwnedWriteHalf) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
        }
    }

    pub async fn write_json_line(&self, value: &Value) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        let mut bytes = serde_json::to_vec(value).map_err(|e| format!("JSON encode: {e}"))?;
        bytes.push(b'\n');
        guard
            .write_all(&bytes)
            .await
            .map_err(|e| format!("write error: {e}"))
    }
}

impl Clone for SharedWriter {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
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
