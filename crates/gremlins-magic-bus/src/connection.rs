//! Connection-scoped RAII guards and a shared socket writer.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

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
// Connection guard — RAII counter via the actor
// ---------------------------------------------------------------------------

/// RAII guard: sends an "up" message on construction and a "down" message on
/// drop.
///
/// Generic over the actor message type `M` so this lives in the bus crate
/// without depending on any particular actor type.
pub struct ConnectionGuard<M: Send + 'static> {
    actor_tx: Option<mpsc::UnboundedSender<M>>,
    down_msg: Option<M>,
}

impl<M: Send + 'static> ConnectionGuard<M> {
    pub fn new(actor_tx: &mpsc::UnboundedSender<M>, up_msg: M, down_msg: M) -> Self {
        let _ = actor_tx.send(up_msg);
        Self {
            actor_tx: Some(actor_tx.clone()),
            down_msg: Some(down_msg),
        }
    }
}

impl<M: Send + 'static> Drop for ConnectionGuard<M> {
    fn drop(&mut self) {
        if let (Some(tx), Some(msg)) = (self.actor_tx.take(), self.down_msg.take()) {
            let _ = tx.send(msg);
        }
    }
}
