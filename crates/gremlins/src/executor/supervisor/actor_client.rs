//! Canonical actor-message client: `ActorMsg` send-and-await wrappers for
//! callers that have been migrated to the `Client` façade. New caller code
//! should reach the actor exclusively through this module; legacy callers
//! (outside this diff) still construct `ActorMsg` inline.
//!
//! Every method follows the same protocol: create a `oneshot` channel, send the
//! variant, await the reply, and map send-failure / reply-drop to the caller's
//! fail-safe default.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use crate::clients::cancel::CancelToken;
use crate::executor::gremlin::Gremlin;

use super::actor::ActorMsg;
use super::actor::{self, ActiveSkills, TurnSnapshot};
use super::CurrentTurn;

/// Lightweight handle wrapping the actor's mailbox sender.
///
/// Every `pub(crate)` method corresponds to exactly one `ActorMsg` variant.
/// The struct is `Clone` (the underlying `UnboundedSender` is cheap to clone).
#[derive(Clone)]
pub(crate) struct Client {
    tx: mpsc::UnboundedSender<ActorMsg>,
}

impl Client {
    /// Wrap an existing actor mailbox sender.
    pub(crate) fn new(tx: mpsc::UnboundedSender<ActorMsg>) -> Self {
        Self { tx }
    }

    // ── Run map ────────────────────────────────────────────────────────

    pub(crate) async fn contains_run(&self, id: &str) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::ContainsRun {
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub(crate) async fn set_run_cancel(&self, id: &str, cancel: Arc<CancelToken>) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SetRunCancel {
                id: id.to_string(),
                cancel,
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    // ── Sessions ───────────────────────────────────────────────────────

    pub(crate) async fn session_is_closed(&self, session_id: &str) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionIsClosed {
                session_id: session_id.to_string(),
                respond,
            })
            .is_err()
        {
            return true;
        }
        rx.await.unwrap_or(true)
    }

    pub(crate) async fn session_add_chat_id(&self, session_id: &str, id: &str) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionAddChatId {
                session_id: session_id.to_string(),
                id: id.to_string(),
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub(crate) async fn session_take_gremlin(&self, session_id: &str, id: &str) -> Option<Gremlin> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::SessionTakeGremlin {
                session_id: session_id.to_string(),
                id: id.to_string(),
                respond,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub(crate) async fn session_reinsert_gremlin(
        &self,
        session_id: &str,
        id: &str,
        gremlin: Gremlin,
    ) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionReinsertGremlin {
                session_id: session_id.to_string(),
                id: id.to_string(),
                gremlin,
                respond,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub(crate) async fn session_get_current_turn(&self, session_id: &str) -> Option<TurnSnapshot> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::SessionGetCurrentTurn {
                session_id: session_id.to_string(),
                respond,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub(crate) async fn session_set_current_turn(&self, session_id: &str, turn: CurrentTurn) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionSetCurrentTurn {
                session_id: session_id.to_string(),
                turn,
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub(crate) async fn session_clear_current_turn(
        &self,
        session_id: &str,
        cancel: Arc<CancelToken>,
    ) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionClearCurrentTurn {
                session_id: session_id.to_string(),
                cancel,
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub(crate) async fn session_cancel_turn(
        &self,
        session_id: &str,
        id: Option<&str>,
        soft: bool,
    ) -> Option<watch::Receiver<bool>> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionCancelTurn {
                session_id: session_id.to_string(),
                id: id.map(String::from),
                soft,
                respond,
            })
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }

    pub(crate) async fn session_update_skills(
        &self,
        session_id: &str,
        requested: Vec<String>,
    ) -> Option<ActiveSkills> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::SessionUpdateSkills {
                session_id: session_id.to_string(),
                requested,
                respond,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub(crate) async fn session_get_chat_gremlin_keys(&self, session_id: &str) -> Vec<String> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionGetChatGremlinKeys {
                session_id: session_id.to_string(),
                respond,
            })
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub(crate) async fn session_get_chat_ids(&self, session_id: &str) -> Vec<String> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionGetChatIds {
                session_id: session_id.to_string(),
                respond,
            })
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub(crate) async fn session_remove_chat_id(&self, session_id: &str, id: &str) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionRemoveChatId {
                session_id: session_id.to_string(),
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub(crate) async fn session_get_all(&self) -> Vec<String> {
        let (respond, rx) = oneshot::channel();
        if self.tx.send(ActorMsg::SessionGetAll { respond }).is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub(crate) async fn session_get_chat_model(&self, session_id: &str) -> Option<String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::SessionGetChatModel {
                session_id: session_id.to_string(),
                respond,
            })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Reserve a `chat-{seq:04x}` id through the actor so it never collides
    /// with a live pipeline run.
    pub(crate) async fn reserve_chat_id(&self) -> Option<String> {
        let (respond, rx) = oneshot::channel();
        self.tx.send(ActorMsg::ReserveChatId { respond }).ok()?;
        rx.await.ok()
    }

    pub(crate) async fn session_mark_closed(&self, session_id: &str) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionMarkClosed {
                session_id: session_id.to_string(),
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub(crate) async fn session_teardown(&self, session_id: &str, abort_listener: bool) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::SessionTeardown {
                session_id: session_id.to_string(),
                abort_listener,
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub(crate) async fn session_push_aux_socket(
        &self,
        session_id: &str,
        task: tokio::task::JoinHandle<()>,
    ) {
        let (respond, _rx) = oneshot::channel();
        let _ = self.tx.send(ActorMsg::SessionPushAuxSocket {
            session_id: session_id.to_string(),
            task,
            respond,
        });
    }

    pub(crate) async fn session_reinsert_and_mark_interrupted(
        &self,
        session_id: &str,
        id: &str,
        gremlin: Gremlin,
        cancel: Arc<CancelToken>,
    ) {
        let (respond, rx) = oneshot::channel();
        let _ = self.tx.send(ActorMsg::SessionReinsertAndMarkInterrupted {
            session_id: session_id.to_string(),
            id: id.to_string(),
            gremlin,
            cancel,
            respond,
        });
        let _ = rx.await;
    }

    pub(crate) async fn register_chat_gremlin(&self, session_id: &str, mut gremlin: Gremlin) {
        let id = gremlin.id.to_string();
        let cancel_token = CancelToken::new();
        // Chat gremlins park a persistent cancel token; each turn swaps in a
        // fresh one via `SetRunCancel`.
        gremlin.cancel_token = Some(cancel_token.clone());
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::RegisterChatGremlin {
                session_id: session_id.to_string(),
                id,
                gremlin,
                cancel: cancel_token,
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub(crate) async fn stop_run(&self, id: &str) -> Option<actor::StoppedRun> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::StopRun {
                id: id.to_string(),
                respond,
            })
            .is_ok()
        {
            rx.await.ok().flatten()
        } else {
            None
        }
    }

    // ── Id locks ───────────────────────────────────────────────────────

    /// Acquire the per-id lock without blocking the actor.
    pub(crate) async fn acquire_id_lock(
        &self,
        id: &str,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        actor::acquire_id_lock(&self.tx, id).await
    }

    // ── Combined liveness + subscribe ──────────────────────────────────

    /// Get the log flush handle *and* a per-run bus subscriber + receiver in
    /// one actor round-trip. The actor checks the run map and calls
    /// `subscribe_run` inside the same handler, closing the TOCTOU window
    /// where the run finishes between the liveness check and the
    /// subscription.
    pub(crate) async fn get_run_log_flush_and_subscribe(
        &self,
        id: &str,
    ) -> Option<actor::FlushAndSubscribe> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::GetRunLogFlushAndSubscribe {
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }
}
