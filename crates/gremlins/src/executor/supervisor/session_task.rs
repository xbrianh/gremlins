//! Per-session mini-actor that owns all session-scoped state.
//!
//! The supervisor used to keep every session in a `sessions` map inside
//! `ActorState`. This module splits that state out into its own task: one
//! [`SessionTaskHandle`] per session, talking to a [`SessionTaskMsg`] mailbox.
//! Session-scoped operations no longer contend with the run map, and the
//! session's state lives and dies with its task.
//!
//! The task's mailbox carries the same request shapes the old actor handled
//! (`SessionTakeGremlin`, `SessionCancelTurn`, ...). The only cross-task
//! dependency is the run map, which now lives in the shared [`RunRegistry`]:
//! operations
//! that must consult it (`ReinsertGremlin`, `GetChatModel`, teardown) do so via
//! the registry's async methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::clients::cancel::CancelToken;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::socket;

use super::chat::CurrentTurn;
use super::chat_handle::ChatGremlinHandle;
use super::lifecycle;
use super::run_registry::RunRegistry;
use super::SessionRegistry;

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// Everything the session task knows about one client session.
///
/// This is the owned counterpart of the old `ClientSession` (which was shared
/// behind an `Arc<tokio::sync::Mutex<..>>`); the task is the only writer, so
/// no lock is needed around it.
pub(crate) struct SessionState {
    #[allow(dead_code)]
    pub session_id: String,
    /// Task that owns the session's `UnixListener`, stashed so teardown can
    /// stop accepting new connections before the socket is unlinked.
    pub listener_task: Option<JoinHandle<()>>,
    /// Chat gremlins parked in the session, keyed by gremlin id. A gremlin is
    /// moved out while a turn runs and re-inserted when the turn finishes.
    pub chat_gremlins: HashMap<String, Gremlin>,
    /// Gremlin ids owned by this session. Kept separate from `chat_gremlins`
    /// because an in-flight turn temporarily removes its gremlin from the map
    /// without relinquishing ownership.
    pub chat_ids: HashSet<String>,
    /// Reserved identity for the first fresh home chat, owned through `chat_ids`.
    pub reserved_home_run_id: Option<String>,
    /// The in-flight chat turn, if any.
    pub current_turn: Option<CurrentTurn>,
    /// Auxiliary socket tasks (log follow, debug) opened on the session
    /// socket. Aborted when the session dies.
    pub aux_sockets: Vec<JoinHandle<()>>,
    /// Set when the main socket reached EOF/error; rejects newly-arriving aux
    /// connections. Distinct from `torn_down`.
    pub closed: bool,
    /// Set when teardown has begun. Makes teardown idempotent.
    pub torn_down: bool,
    /// Skill names the operator explicitly requested on the last turn.
    pub operator_skills: Vec<String>,
    /// Skill names loaded by the model during the session, accumulated across
    /// turns.
    pub model_loaded_skills: Vec<String>,
    /// The session's persistent chat-gremlin task handle, if one has been
    /// spawned. Dropped on teardown, which closes the prompt channel and lets
    /// the task exit.
    pub chat_handle: Option<ChatGremlinHandle>,
    /// State root, used to unlink the session socket on teardown. Owned by the
    /// session task because teardown is now session-local.
    pub state_root: PathBuf,
}

impl SessionState {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            listener_task: None,
            chat_gremlins: HashMap::new(),
            chat_ids: HashSet::new(),
            reserved_home_run_id: None,
            current_turn: None,
            aux_sockets: Vec::new(),
            closed: false,
            torn_down: false,
            operator_skills: Vec::new(),
            model_loaded_skills: Vec::new(),
            chat_handle: None,
            state_root: PathBuf::new(),
        }
    }

    /// True when the session can accept a new home chat prompt:
    /// no turn is in-flight. A fresh gremlin can be created if
    /// none is parked yet (first prompt).
    fn is_idle_for_chat(&self) -> bool {
        self.current_turn.is_none()
    }

    /// Cancel the current turn, optionally filtered to `id`. Returns the
    /// turn's `done` receiver so the caller can wait for completion.
    fn cancel_turn(&self, id: Option<&str>) -> Option<watch::Receiver<bool>> {
        let cur = self.current_turn.as_ref()?;
        if let Some(id) = id {
            if cur.id != id {
                return None;
            }
        }
        if !cur.cancel.is_cancelled() {
            cur.cancel.cancel();
        }
        Some(cur.done.clone())
    }

    /// Take everything a teardown needs out of the session. Idempotent: a
    /// session already torn down yields `None`.
    fn take_teardown(&mut self, abort_listener: bool) -> Option<TeardownJob> {
        if self.torn_down {
            return None;
        }
        self.torn_down = true;
        self.closed = true;

        // Cancel (and get a handle on) the in-flight turn before dropping the
        // session — the turn task signals `done` through its own `Arc`.
        let turn_done = self.cancel_turn(None);

        self.reserved_home_run_id = None;
        let chat_ids: Vec<String> = self.chat_ids.drain().collect();
        let chat_gremlins = std::mem::take(&mut self.chat_gremlins);
        let aux_sockets = std::mem::take(&mut self.aux_sockets);
        let mut listener_task = self.listener_task.take();
        if !abort_listener {
            // The listener is already exiting on its own; just drop the handle.
            listener_task = None;
        }

        Some(TeardownJob {
            session_id: self.session_id.clone(),
            state_root: self.state_root.clone(),
            listener_task,
            aux_sockets,
            chat_ids,
            chat_gremlins,
            turn_done,
        })
    }

    /// Resolve the model in use by a session's chat gremlin — parked first,
    /// then live in the run map (an in-flight turn parks its gremlin outside
    /// the session).
    async fn session_chat_model(&self, registry: &Arc<RunRegistry>) -> Option<String> {
        let id = self
            .chat_gremlins
            .keys()
            .next()
            .cloned()
            .or_else(|| self.current_turn.as_ref().map(|turn| turn.id.clone()))?;
        let parked = self
            .chat_gremlins
            .get(&id)
            .map(|gremlin| gremlin.state.read_str("model"))
            .filter(|model| !model.is_empty());
        if parked.is_some() {
            return parked;
        }
        // Run-map fallback: the gremlin is mid-turn, so its state lives in the
        // shared registry rather than in the session.
        let store = registry.get_run_store(&id).await?;
        let model = store.read_str("model");
        if model.is_empty() {
            None
        } else {
            Some(model)
        }
    }
}

/// The fields of a session's in-flight turn that callers outside the task
/// need. Deliberately a snapshot: `CurrentTurn` itself stays task-owned.
pub(crate) struct TurnSnapshot {
    pub id: String,
    pub turn: Option<String>,
    pub done: watch::Receiver<bool>,
    /// The opaque stage-run ID the turn announced at start, if any.
    pub stage_run_id: Option<String>,
}

/// Work extracted by `Teardown` and finished by a task spawned from the
/// session loop, so the loop stays responsive while a turn drains.
pub(crate) struct TeardownJob {
    pub(crate) session_id: String,
    pub(crate) state_root: PathBuf,
    pub(crate) listener_task: Option<JoinHandle<()>>,
    pub(crate) aux_sockets: Vec<JoinHandle<()>>,
    pub(crate) chat_ids: Vec<String>,
    pub(crate) chat_gremlins: HashMap<String, Gremlin>,
    pub(crate) turn_done: Option<watch::Receiver<bool>>,
}

// ---------------------------------------------------------------------------
// Chat-handle operations
// ---------------------------------------------------------------------------

/// Operation on a session's chat-gremlin handle.
pub(crate) enum ChatHandleOp {
    /// Store (or clear with `None`) the handle.
    Set(Option<ChatGremlinHandle>),
    /// Fetch a clone without removing.
    Get,
    /// Take ownership (removes from session, leaves None).
    Take,
    /// Cancel the handle's cancel token *only if* its id matches.
    CancelIf(String),
}

/// Reply from a [`ChatHandleOp`].
pub(crate) enum ChatHandleReply {
    SetAck,
    Get(Option<ChatGremlinHandle>),
    Take(Option<ChatGremlinHandle>),
    CancelIf(bool),
}

/// The reply a [`ChatHandleOp`] yields when the session task is gone.
fn chat_handle_default(op: &ChatHandleOp) -> ChatHandleReply {
    match op {
        ChatHandleOp::Set(_) => ChatHandleReply::SetAck,
        ChatHandleOp::Get => ChatHandleReply::Get(None),
        ChatHandleOp::Take => ChatHandleReply::Take(None),
        ChatHandleOp::CancelIf(_) => ChatHandleReply::CancelIf(false),
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// One request or notification for a session task.
///
/// Variants that need a reply carry a `oneshot::Sender`. The enum is large on
/// purpose: `ReinsertGremlin` moves a whole [`Gremlin`] back into the session.
#[allow(clippy::large_enum_variant)]
pub(crate) enum SessionTaskMsg {
    /// Move a parked gremlin out of the session (a turn is starting).
    TakeGremlin {
        id: String,
        respond: oneshot::Sender<Option<Gremlin>>,
    },
    /// Re-insert a finished turn's gremlin if the session still owns it and
    /// the run-map entry is still live. `false` means the gremlin was dropped.
    ReinsertGremlin {
        id: String,
        gremlin: Gremlin,
        respond: oneshot::Sender<bool>,
    },
    GetCurrentTurn {
        respond: oneshot::Sender<Option<TurnSnapshot>>,
    },
    SetCurrentTurn {
        turn: CurrentTurn,
        respond: oneshot::Sender<()>,
    },
    /// Clear `current_turn` if it still points at `cancel` (Arc identity).
    ClearCurrentTurn {
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<bool>,
    },
    /// Cancel a turn: `id` filters by gremlin id. Returns the turn's `done`
    /// receiver.
    CancelTurn {
        id: Option<String>,
        respond: oneshot::Sender<Option<watch::Receiver<bool>>>,
    },
    /// Set the hard-cancel flag on the session's current turn so the turn's
    /// event loop knows the operator hit double-Esc (hard).
    SetTurnHard {
        respond: oneshot::Sender<()>,
    },
    IsClosed {
        respond: oneshot::Sender<bool>,
    },
    /// Check whether the session is idle for a new home chat prompt.
    IsIdleForChat {
        respond: oneshot::Sender<bool>,
    },
    /// Mark the session closed and cancel the in-flight turn.
    MarkClosed {
        respond: oneshot::Sender<()>,
    },
    ReserveHomeRunId {
        id: String,
        respond: oneshot::Sender<bool>,
    },
    GetHomeRunId {
        respond: oneshot::Sender<Option<String>>,
    },
    AddChatId {
        id: String,
        respond: oneshot::Sender<bool>,
    },
    RemoveChatId {
        id: String,
        respond: oneshot::Sender<bool>,
    },
    GetChatIds {
        respond: oneshot::Sender<Vec<String>>,
    },
    GetChatGremlinKeys {
        respond: oneshot::Sender<Vec<String>>,
    },
    /// Operate on the session's persistent chat-gremlin task handle: store,
    /// fetch a clone, take ownership, or cancel-if-id-matches (a single atomic
    /// operation: no caller-side take-then-reinsert window for an interleaved
    /// `handle_chat` to install a fresh handle).
    ChatHandleOp {
        op: ChatHandleOp,
        respond: oneshot::Sender<ChatHandleReply>,
    },
    /// Store the operator-requested skill names for this session.
    SetOperatorSkills {
        skill_names: Vec<String>,
        respond: oneshot::Sender<()>,
    },
    /// Append model-loaded skill names to the session's accumulated set.
    AppendModelSkills {
        skill_names: Vec<String>,
        respond: oneshot::Sender<()>,
    },
    /// Return the session's combined active skill names: operator + model.
    GetCombinedSkills {
        respond: oneshot::Sender<Option<Vec<String>>>,
    },
    /// Tear the session down. The reply fires once the drain has finished; the
    /// drain itself runs in a task spawned by this loop (see module docs).
    Teardown {
        abort_listener: bool,
        respond: oneshot::Sender<()>,
    },
    SetListenerTask {
        task: JoinHandle<()>,
        respond: oneshot::Sender<()>,
    },
    PushAuxSocket {
        task: JoinHandle<()>,
        respond: oneshot::Sender<()>,
    },
    /// The model of the session's chat gremlin, live or parked.
    GetChatModel {
        respond: oneshot::Sender<Option<String>>,
    },
}

// ---------------------------------------------------------------------------
// SessionTaskHandle
// ---------------------------------------------------------------------------

/// Lightweight sender for a session task's mailbox.
///
/// Every method is fail-safe: if the task is gone (its mailbox closed), the
/// call returns the "session is gone" answer rather than panicking, so callers
/// never have to distinguish "no such session" from "session task died".
#[derive(Clone)]
pub(crate) struct SessionTaskHandle {
    tx: mpsc::UnboundedSender<SessionTaskMsg>,
    #[cfg(test)]
    turn_gates: Arc<tokio::sync::Mutex<HashMap<(String, &'static str), TestTurnGate>>>,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestTurnGate {
    pub(crate) entered: Arc<tokio::sync::Notify>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

impl SessionTaskHandle {
    fn new(tx: mpsc::UnboundedSender<SessionTaskMsg>) -> Self {
        Self {
            tx,
            #[cfg(test)]
            turn_gates: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn gate_turn(&self, turn: &str, phase: &'static str) -> TestTurnGate {
        let gate = TestTurnGate::default();
        self.turn_gates
            .lock()
            .await
            .insert((turn.into(), phase), gate.clone());
        gate
    }

    #[cfg(test)]
    pub(crate) async fn wait_turn_gate(&self, turn: Option<&str>, phase: &'static str) {
        let gate = self
            .turn_gates
            .lock()
            .await
            .remove(&(turn.unwrap_or_default().into(), phase));
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }

    /// Move a parked gremlin out of the session. `None` if the session is
    /// gone or owns no gremlin by that id.
    pub(crate) async fn take_gremlin(&self, id: &str) -> Option<Gremlin> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::TakeGremlin {
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    /// Re-insert a finished turn's gremlin. `false` if the session is gone or
    /// no longer owns the id.
    pub(crate) async fn reinsert_gremlin(&self, id: &str, gremlin: Gremlin) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::ReinsertGremlin {
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

    pub(crate) async fn get_current_turn(&self) -> Option<TurnSnapshot> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::GetCurrentTurn { respond })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    pub(crate) async fn set_current_turn(&self, turn: CurrentTurn) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::SetCurrentTurn { turn, respond })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    pub(crate) async fn clear_current_turn(&self, cancel: Arc<CancelToken>) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::ClearCurrentTurn { cancel, respond })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    /// Cancel the current turn, optionally filtered to `id`. Returns the
    /// turn's `done` receiver so the caller can wait for completion.
    pub(crate) async fn cancel_turn(&self, id: Option<&str>) -> Option<watch::Receiver<bool>> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::CancelTurn {
                id: id.map(str::to_string),
                respond,
            })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    pub(crate) async fn set_turn_hard(&self) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::SetTurnHard { respond })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    /// `true` when the session is gone or has been marked closed.
    pub(crate) async fn is_closed(&self) -> bool {
        let (respond, rx) = oneshot::channel();
        if self.tx.send(SessionTaskMsg::IsClosed { respond }).is_err() {
            return true;
        }
        rx.await.unwrap_or(true)
    }

    pub(crate) async fn mark_closed(&self) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::MarkClosed { respond })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    /// Claim ownership of `id`. `false` when the session task is gone or has
    /// already been closed/torn down, so a concurrent teardown cannot race a
    /// freshly-spawned chat gremlin into a dead session.
    pub(crate) async fn add_chat_id(&self, id: &str) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::AddChatId {
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub(crate) async fn reserve_home_run_id(&self, id: &str) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::ReserveHomeRunId {
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Read the reservation without consuming it; successful parking commits it.
    pub(crate) async fn get_home_run_id(&self) -> Option<String> {
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(SessionTaskMsg::GetHomeRunId { respond })
            .ok()?;
        rx.await.ok().flatten()
    }

    pub(crate) async fn remove_chat_id(&self, id: &str) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::RemoveChatId {
                id: id.to_string(),
                respond,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub(crate) async fn get_chat_ids(&self) -> Vec<String> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::GetChatIds { respond })
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub(crate) async fn get_chat_gremlin_keys(&self) -> Vec<String> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::GetChatGremlinKeys { respond })
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub(crate) async fn chat_handle_op(&self, op: ChatHandleOp) -> ChatHandleReply {
        let default = chat_handle_default(&op);
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::ChatHandleOp { op, respond })
            .is_err()
        {
            return default;
        }
        rx.await.unwrap_or(default)
    }

    /// Store (or clear) the session's chat-gremlin handle.
    pub(crate) async fn set_chat_handle(&self, handle: Option<ChatGremlinHandle>) {
        let _ = self.chat_handle_op(ChatHandleOp::Set(handle)).await;
    }

    /// Take ownership of the session's chat-gremlin handle, leaving `None`.
    pub(crate) async fn take_chat_handle(&self) -> Option<ChatGremlinHandle> {
        match self.chat_handle_op(ChatHandleOp::Take).await {
            ChatHandleReply::Take(handle) => handle,
            _ => None,
        }
    }

    /// Cancel the session's chat-gremlin handle only if its id matches.
    pub(crate) async fn cancel_chat_handle_if(&self, id: &str) -> bool {
        match self
            .chat_handle_op(ChatHandleOp::CancelIf(id.to_string()))
            .await
        {
            ChatHandleReply::CancelIf(cancelled) => cancelled,
            _ => false,
        }
    }

    /// Fetch a clone of the session's chat-gremlin handle, without removing it.
    pub(crate) async fn get_chat_handle(&self) -> Option<ChatGremlinHandle> {
        match self.chat_handle_op(ChatHandleOp::Get).await {
            ChatHandleReply::Get(handle) => handle,
            _ => None,
        }
    }

    pub(crate) async fn set_operator_skills(&self, skill_names: Vec<String>) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::SetOperatorSkills {
                skill_names,
                respond,
            })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    pub(crate) async fn append_model_skills(&self, skill_names: Vec<String>) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::AppendModelSkills {
                skill_names,
                respond,
            })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    pub(crate) async fn get_combined_skills(&self) -> Option<Vec<String>> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::GetCombinedSkills { respond })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    pub(crate) async fn teardown(&self, abort_listener: bool) {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::Teardown {
                abort_listener,
                respond,
            })
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    /// Stash the listener task. Aborts it if the session task is already gone
    /// (no one left to own the handle).
    pub(crate) async fn set_listener_task(&self, task: JoinHandle<()>) {
        let (respond, rx) = oneshot::channel();
        if let Err(err) = self
            .tx
            .send(SessionTaskMsg::SetListenerTask { task, respond })
        {
            if let SessionTaskMsg::SetListenerTask { task, .. } = err.0 {
                task.abort();
            }
            return;
        }
        let _ = rx.await;
    }

    /// Stash an aux socket task. Aborts it if the session task is gone.
    pub(crate) async fn push_aux_socket(&self, task: JoinHandle<()>) {
        let (respond, rx) = oneshot::channel();
        if let Err(err) = self
            .tx
            .send(SessionTaskMsg::PushAuxSocket { task, respond })
        {
            if let SessionTaskMsg::PushAuxSocket { task, .. } = err.0 {
                task.abort();
            }
            return;
        }
        let _ = rx.await;
    }

    /// `true` while the session task's mailbox receiver is still alive.
    /// Unlike [`Self::is_closed`], this reports the task's existence rather
    /// than the session's `closed` flag (EOF marks a session closed without
    /// ending its task until teardown drains).
    #[allow(dead_code)]
    pub(crate) fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }

    pub(crate) async fn get_chat_model(&self) -> Option<String> {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::GetChatModel { respond })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    /// `true` when the session is idle (chat gremlin parked, no turn
    /// in-flight) and can accept a new home chat prompt.
    pub(crate) async fn is_idle_for_chat(&self) -> bool {
        let (respond, rx) = oneshot::channel();
        if self
            .tx
            .send(SessionTaskMsg::IsIdleForChat { respond })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Spawn / loop
// ---------------------------------------------------------------------------

/// Spawn a session task and return its mailbox handle.
pub(crate) fn spawn_session_task(
    state_root: PathBuf,
    session_id: String,
    registry: Arc<RunRegistry>,
    sessions: Option<SessionRegistry>,
) -> SessionTaskHandle {
    let (tx, msg_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        run_session_task(msg_rx, state_root, session_id, registry, sessions).await;
    });
    SessionTaskHandle::new(tx)
}

/// The session loop: process one message at a time, forever (or until
/// teardown).
async fn run_session_task(
    mut msg_rx: mpsc::UnboundedReceiver<SessionTaskMsg>,
    state_root: PathBuf,
    session_id: String,
    registry: Arc<RunRegistry>,
    sessions: Option<SessionRegistry>,
) {
    let mut state = SessionState::new(session_id.clone());
    state.state_root = state_root;

    while let Some(msg) = msg_rx.recv().await {
        match msg {
            SessionTaskMsg::TakeGremlin { id, respond } => {
                let gremlin = state.chat_gremlins.remove(&id);
                let _ = respond.send(gremlin);
            }

            SessionTaskMsg::ReinsertGremlin {
                id,
                gremlin,
                respond,
            } => {
                // Re-insert only if the session still owns the id *and* the
                // run-map entry is still live. The run map lives in the shared
                // registry, so ask it before mutating our own state.
                let owned = state.chat_ids.contains(&id);
                let live = owned && registry.contains_run(&id).await;
                // Re-check ownership after the registry await: this handler
                // awaits, so other session messages (e.g. `RemoveChatId` from
                // a control-side stop or a racing teardown) can be processed
                // while we wait and invalidate the earlier check.
                let reinserted = if live && state.chat_ids.contains(&id) {
                    // Commit the home identity only once registration succeeds.
                    if state.reserved_home_run_id.as_deref() == Some(id.as_str()) {
                        state.reserved_home_run_id = None;
                    }
                    state.chat_gremlins.insert(id, gremlin);
                    true
                } else {
                    false
                };
                let _ = respond.send(reinserted);
            }

            SessionTaskMsg::GetCurrentTurn { respond } => {
                let turn = state.current_turn.as_ref().map(|cur| TurnSnapshot {
                    id: cur.id.clone(),
                    turn: cur.turn.clone(),
                    done: cur.done.clone(),
                    stage_run_id: cur.stage_run_id.clone(),
                });
                let _ = respond.send(turn);
            }

            SessionTaskMsg::SetCurrentTurn { turn, respond } => {
                state.current_turn = Some(turn);
                let _ = respond.send(());
            }

            SessionTaskMsg::ClearCurrentTurn { cancel, respond } => {
                let matches = state
                    .current_turn
                    .as_ref()
                    .is_some_and(|cur| Arc::ptr_eq(&cur.cancel, &cancel));
                if matches {
                    state.current_turn = None;
                }
                let _ = respond.send(matches);
            }

            SessionTaskMsg::CancelTurn { id, respond } => {
                let done = state.cancel_turn(id.as_deref());
                let _ = respond.send(done);
            }

            SessionTaskMsg::SetTurnHard { respond } => {
                if let Some(ref turn) = state.current_turn {
                    turn.hard.store(true, Ordering::Relaxed);
                }
                let _ = respond.send(());
            }

            SessionTaskMsg::IsClosed { respond } => {
                let _ = respond.send(state.closed);
            }

            SessionTaskMsg::IsIdleForChat { respond } => {
                let _ = respond.send(state.is_idle_for_chat());
            }

            SessionTaskMsg::MarkClosed { respond } => {
                state.closed = true;
                if let Some(cur) = &state.current_turn {
                    cur.cancel.cancel();
                }
                let _ = respond.send(());
            }

            SessionTaskMsg::ReserveHomeRunId { id, respond } => {
                let accepted =
                    !state.closed && !state.torn_down && state.reserved_home_run_id.is_none();
                if accepted {
                    state.chat_ids.insert(id.clone());
                    state.reserved_home_run_id = Some(id);
                }
                let _ = respond.send(accepted);
            }

            SessionTaskMsg::GetHomeRunId { respond } => {
                let _ = respond.send(state.reserved_home_run_id.clone());
            }

            SessionTaskMsg::AddChatId { id, respond } => {
                let accepted = !state.torn_down && !state.closed;
                if accepted {
                    state.chat_ids.insert(id);
                }
                let _ = respond.send(accepted);
            }

            SessionTaskMsg::RemoveChatId { id, respond } => {
                if state.reserved_home_run_id.as_deref() == Some(&id) {
                    state.reserved_home_run_id = None;
                }
                let _ = respond.send(state.chat_ids.remove(&id));
            }

            SessionTaskMsg::GetChatIds { respond } => {
                let ids = state.chat_ids.iter().cloned().collect();
                let _ = respond.send(ids);
            }

            SessionTaskMsg::GetChatGremlinKeys { respond } => {
                let keys = state.chat_gremlins.keys().cloned().collect();
                let _ = respond.send(keys);
            }

            SessionTaskMsg::ChatHandleOp { op, respond } => {
                let reply = match op {
                    ChatHandleOp::Set(handle) => {
                        state.chat_handle = handle;
                        ChatHandleReply::SetAck
                    }
                    ChatHandleOp::Get => ChatHandleReply::Get(state.chat_handle.clone()),
                    ChatHandleOp::Take => ChatHandleReply::Take(state.chat_handle.take()),
                    ChatHandleOp::CancelIf(id) => {
                        let matches = state
                            .chat_handle
                            .as_ref()
                            .is_some_and(|handle| handle.id == id);
                        if matches {
                            if let Some(handle) = state.chat_handle.take() {
                                handle.cancel.cancel();
                            }
                        }
                        ChatHandleReply::CancelIf(matches)
                    }
                };
                let _ = respond.send(reply);
            }

            SessionTaskMsg::SetOperatorSkills {
                skill_names,
                respond,
            } => {
                state.operator_skills = skill_names;
                let _ = respond.send(());
            }

            SessionTaskMsg::AppendModelSkills {
                skill_names,
                respond,
            } => {
                for name in skill_names {
                    if !state.model_loaded_skills.contains(&name) {
                        state.model_loaded_skills.push(name);
                    }
                }
                let _ = respond.send(());
            }

            SessionTaskMsg::GetCombinedSkills { respond } => {
                let mut combined = state.operator_skills.clone();
                for name in &state.model_loaded_skills {
                    if !combined.contains(name) {
                        combined.push(name.clone());
                    }
                }
                let _ = respond.send(Some(combined));
            }

            SessionTaskMsg::Teardown {
                abort_listener,
                respond,
            } => {
                let Some(job) = state.take_teardown(abort_listener) else {
                    let _ = respond.send(());
                    return;
                };
                // Deregister before the drain so control-socket cross-session
                // ops stop routing to a session that is already tearing down.
                if let Some(sessions) = &sessions {
                    sessions.lock().unwrap().remove(&session_id);
                }
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    complete_teardown(job, registry).await;
                    let _ = respond.send(());
                });
                break;
            }

            SessionTaskMsg::SetListenerTask { task, respond } => {
                state.listener_task = Some(task);
                let _ = respond.send(());
            }

            SessionTaskMsg::PushAuxSocket { task, respond } => {
                if !state.closed {
                    state.aux_sockets.push(task);
                } else {
                    task.abort();
                }
                let _ = respond.send(());
            }

            SessionTaskMsg::GetChatModel { respond } => {
                let model = state.session_chat_model(&registry).await;
                let _ = respond.send(model);
            }
        }
    }
}

/// Finish a teardown outside the session loop.
///
/// Order matters and mirrors the old `teardown_session_impl`: stop the
/// listener, abort aux sockets (so their connection guards run), wait for the
/// cancelled turn to relinquish its gremlin, then reap the chat gremlins and
/// drop the parked ones.
async fn complete_teardown(job: TeardownJob, registry: Arc<RunRegistry>) {
    if let Some(listener_task) = job.listener_task {
        listener_task.abort();
    }

    for aux in job.aux_sockets {
        aux.abort();
        let _ = aux.await;
    }

    if let Some(mut done) = job.turn_done {
        let _ = done.wait_for(|v| *v).await;
    }

    for id in &job.chat_ids {
        if validate_gremlin_id(id).is_err() {
            continue;
        }
        // Stop first so the registry removes the run-map entry and publishes
        // the terminal state; the reap below must not race a still-live handle.
        let _ = registry.stop_run(id).await;
        lifecycle::terminate_and_reap(id);
    }

    // Drop parked gremlins only after their run-map entries are gone.
    drop(job.chat_gremlins);

    // The session is gone from the map; the socket is dead weight now.
    socket::unlink_session_socket(&job.state_root, &job.session_id);
    log::debug!("session_task: {} torn down", job.session_id);
}
