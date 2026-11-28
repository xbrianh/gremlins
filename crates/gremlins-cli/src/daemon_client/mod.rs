use std::sync::Arc;

use gremlins_magic_bus::EventKind;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::tui::client::{self, ChatEvent, DaemonClient};

/// Client counterpart of the daemon's `SessionTaskHandle`.
/// This session is the only object allowed to talk to the daemon; for now it
/// delegates operations to the existing `DaemonClient`.
pub(crate) struct DaemonSession {
    client: Arc<DaemonClient>,
    event_rx: Option<mpsc::UnboundedReceiver<EventKind>>,
    chat_rx: Option<mpsc::UnboundedReceiver<ChatEvent>>,
    raw_rx: Option<mpsc::UnboundedReceiver<Value>>,
    closed: bool,
}

impl DaemonSession {
    pub(crate) async fn connect() -> Result<Self, String> {
        let (client, event_rx, chat_rx, raw_rx) = client::connect().await?;
        Ok(Self::new(client, event_rx, chat_rx, raw_rx))
    }

    fn new(
        client: DaemonClient,
        event_rx: mpsc::UnboundedReceiver<EventKind>,
        chat_rx: mpsc::UnboundedReceiver<ChatEvent>,
        raw_rx: mpsc::UnboundedReceiver<Value>,
    ) -> Self {
        Self {
            client: Arc::new(client),
            event_rx: Some(event_rx),
            chat_rx: Some(chat_rx),
            raw_rx: Some(raw_rx),
            closed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_client(
        client: DaemonClient,
        event_rx: mpsc::UnboundedReceiver<EventKind>,
        chat_rx: mpsc::UnboundedReceiver<ChatEvent>,
        raw_rx: mpsc::UnboundedReceiver<Value>,
    ) -> Self {
        Self::new(client, event_rx, chat_rx, raw_rx)
    }

    pub(crate) fn client(&self) -> &Arc<DaemonClient> {
        &self.client
    }

    pub(crate) fn take_event_rx(&mut self) -> mpsc::UnboundedReceiver<EventKind> {
        assert!(
            !self.closed,
            "cannot take event receiver after session close"
        );
        self.event_rx.take().expect("event receiver already taken")
    }

    pub(crate) fn take_chat_rx(&mut self) -> mpsc::UnboundedReceiver<ChatEvent> {
        assert!(
            !self.closed,
            "cannot take chat receiver after session close"
        );
        self.chat_rx.take().expect("chat receiver already taken")
    }

    pub(crate) fn take_raw_rx(&mut self) -> mpsc::UnboundedReceiver<Value> {
        assert!(!self.closed, "cannot take raw receiver after session close");
        self.raw_rx.take().expect("raw receiver already taken")
    }

    pub(crate) async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.event_rx.take();
        self.chat_rx.take();
        self.raw_rx.take();
        self.client.release_inputs();
        self.client.shutdown().await;
        self.closed = true;
    }
}
