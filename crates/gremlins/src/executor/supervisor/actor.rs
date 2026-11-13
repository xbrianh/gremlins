//! Single-threaded actor that owns all supervisor state.
//!
//! Everything that used to live in process-global statics — the run map, the
//! session map, the connection counter and the per-id locks — lives here, in
//! one task, reachable only through [`ActorMsg`]. Because the actor processes
//! one message at a time, every state transition is serialized: there is no
//! lock and no TOCTOU window between "is this id live?" and "insert/remove
//! this id".
//!
//! Every request variant carries a `oneshot::Sender` so callers can await the
//! reply. Fire-and-forget messages (`RunFinished`, `ConnectionUp`,
//! `ConnectionDown`, `Shutdown`) have no reply — they are sent from `Drop`
//! impls and spawned tasks that must never block.
//!
//! ## Why some work is handed back to the caller
//!
//! A few operations have to `.await` something that itself needs the actor
//! (a lease on the id lock, a chat turn's `done` watch, aux sockets draining).
//! Awaiting those *inside* `handle_message` would deadlock — the awaited task
//! is waiting for a reply the actor can only produce after it stops blocking.
//! Those paths (`AcquireIdLock`, `SessionTeardown`) therefore do the state
//! surgery here and hand the waiting to the caller (or to a task spawned from
//! here), which can await freely while the actor keeps serving messages.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::clients::cancel::CancelToken;
use crate::clients::interactive::{InteractiveCommand, InteractiveHandle, StreamEventSender};
use crate::executor::event::EventKind;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::socket;
use crate::executor::state;
use crate::magic_bus::Subscriber;

use super::event::{emit_event, init_bus};
use super::lifecycle::{run_gremlin_task, setup_gremlin_channels, LaunchResult, LogRelay};
use super::state_view::definition_name_from_tree;
use super::{CurrentTurn, RunHandle, RunState};
/// Response payload for [`ActorMsg::ListRunStoresWithIds`].
type ListRunStoresWithIdsResponse = (Vec<String>, Vec<(String, StoreHandle)>);

/// The state store handle a live run exposes to the query ops.
pub(crate) type StoreHandle = Arc<dyn state::StateStore + Send + Sync>;

/// Flush barrier for a run's log writer.
pub(crate) type LogFlushHandle = mpsc::UnboundedSender<oneshot::Sender<()>>;

/// What [`ActorMsg::StopRun`] hands back when the id was live: the run's state
/// broadcaster and its log subscription, so the caller can finish
/// `terminate_and_reap` and release the log task.
pub(crate) type StoppedRun = (watch::Sender<RunState>, Option<Subscriber<EventKind>>);

/// Result of [`ActorMsg::SessionUpdateSkills`]: the active skill names and the
/// shared set the agent loop reads.
pub(crate) type ActiveSkills = (Vec<String>, Arc<Mutex<Vec<String>>>);

/// The actor's mailbox sender. All callers outside this module reach the
/// supervisor through this lightweight handle.
#[allow(dead_code)]
pub(crate) type ActorSender = mpsc::UnboundedSender<ActorMsg>;

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// Everything the actor knows about one client session.
///
/// This is the owned counterpart of the old `ClientSession` (which was shared
/// behind an `Arc<tokio::sync::Mutex<..>>`); the actor is the only writer, so
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
    /// Session-scoped set of active skill names, shared with the agent loop so
    /// the LoadSkill tool can add names mid-turn.
    pub active_skills: Arc<Mutex<Vec<String>>>,
    /// Skill names the operator explicitly requested on the last turn.
    pub operator_skills: Vec<String>,
}

impl SessionState {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            listener_task: None,
            chat_gremlins: HashMap::new(),
            chat_ids: HashSet::new(),
            current_turn: None,
            aux_sockets: Vec::new(),
            closed: false,
            torn_down: false,
            active_skills: Arc::new(Mutex::new(Vec::new())),
            operator_skills: Vec::new(),
        }
    }

    /// Cancel the current turn, optionally filtered to `id`. A soft request is
    /// ignored once the token has already been cancelled so a late soft cancel
    /// cannot overwrite an earlier hard one.
    fn cancel_turn(&self, id: Option<&str>, soft: bool) -> Option<watch::Receiver<bool>> {
        let cur = self.current_turn.as_ref()?;
        if let Some(id) = id {
            if cur.id != id {
                return None;
            }
        }
        let already_cancelled = cur.cancel.is_cancelled();
        if soft {
            if !already_cancelled {
                cur.soft_cancel.store(true, Ordering::Release);
            }
        } else {
            cur.soft_cancel.store(false, Ordering::Release);
        }
        if !already_cancelled {
            cur.cancel.cancel();
        }
        Some(cur.done.clone())
    }
}

/// The fields of a session's in-flight turn that callers outside the actor
/// need. Deliberately a snapshot: `CurrentTurn` itself stays actor-owned.
pub(crate) struct TurnSnapshot {
    pub id: String,
    pub turn: Option<String>,
    pub cancel: Arc<CancelToken>,
    pub done: watch::Receiver<bool>,
    pub interrupted: bool,
}

/// Work extracted by `SessionTeardown` and finished by a task spawned from the
/// actor loop, so the loop stays responsive while a turn drains.
pub(crate) struct TeardownJob {
    session_id: String,
    state_root: PathBuf,
    listener_task: Option<JoinHandle<()>>,
    aux_sockets: Vec<JoinHandle<()>>,
    chat_ids: Vec<String>,
    chat_gremlins: HashMap<String, Gremlin>,
    turn_done: Option<watch::Receiver<bool>>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// One request or notification for the actor.
///
/// Variants that need a reply carry a `oneshot::Sender`. The enum is large on
/// purpose: `Launch*` variants move a whole [`Gremlin`] into the actor, which
/// is a single allocation-heavy send per launch.
#[allow(clippy::large_enum_variant)]
pub(crate) enum ActorMsg {
    // ── Run map ────────────────────────────────────────────────────────
    /// Start a fresh top-level run (replaces `spawn_gremlin_runner`).
    LaunchRun {
        gremlin: Gremlin,
        stage_label: String,
        resume_from: Option<String>,
        definition_name: String,
        /// Whether a terminal state may trigger idle shutdown (top-level runs
        /// may; parallel children may not, their parent is still running).
        notify_shutdown: bool,
        respond: oneshot::Sender<String>,
    },
    /// Launch a child gremlin for the parallel executor (replaces
    /// `launch_child`).
    LaunchChild {
        gremlin: Gremlin,
        relay: Option<LogRelay>,
        respond: oneshot::Sender<LaunchResult>,
    },
    /// A spawned run task finished. Fire-and-forget.
    RunFinished {
        id: String,
        result: i32,
        notify_shutdown: bool,
    },
    /// Stop a run and hand the caller what it needs for follow-up work
    /// (`terminate_and_reap`). `None` means the id was not live.
    StopRun {
        id: String,
        respond: oneshot::Sender<Option<StoppedRun>>,
    },
    ContainsRun {
        id: String,
        respond: oneshot::Sender<bool>,
    },
    GetRunStore {
        id: String,
        respond: oneshot::Sender<Option<StoreHandle>>,
    },
    GetRunInteractive {
        id: String,
        respond: oneshot::Sender<Option<InteractiveHandle>>,
    },
    #[allow(dead_code)]
    GetRunStageStream {
        id: String,
        respond: oneshot::Sender<Option<tokio::sync::broadcast::Sender<String>>>,
    },
    /// Fetch both stage-stream broadcast and log-flush in one round-trip.
    GetRunStageStreamAndFlush {
        id: String,
        respond: oneshot::Sender<(
            Option<tokio::sync::broadcast::Sender<String>>,
            Option<LogFlushHandle>,
        )>,
    },
    /// Replace a live run's interactive command sender (the agent loop swaps in
    /// a fresh channel when it starts; `handle_debug` must reach the new one).
    SetRunInteractiveCmd {
        id: String,
        cmd_tx: mpsc::Sender<InteractiveCommand>,
        respond: oneshot::Sender<bool>,
    },
    GetRunLogFlush {
        id: String,
        respond: oneshot::Sender<Option<LogFlushHandle>>,
    },
    #[allow(dead_code)]
    GetRunCancel {
        id: String,
        respond: oneshot::Sender<Option<Arc<CancelToken>>>,
    },
    #[allow(dead_code)]
    ListRunStores {
        respond: oneshot::Sender<Vec<(String, StoreHandle)>>,
    },
    #[allow(dead_code)]
    ListRunIds {
        respond: oneshot::Sender<Vec<String>>,
    },
    /// Combined: ids + stores in one actor round-trip (no TOCTOU window).
    ListRunStoresWithIds {
        respond: oneshot::Sender<ListRunStoresWithIdsResponse>,
    },
    /// Swap a run's cancel token (chat turns install a fresh per-turn token).
    SetRunCancel {
        id: String,
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<bool>,
    },
    /// Insert a chat gremlin's run-map entry *without* spawning a task (a chat
    /// gremlin has no `run_gremlin_task`; its turn runs in `handle_chat`) and
    /// park the gremlin in its session.
    RegisterChatGremlin {
        session_id: String,
        id: String,
        gremlin: Gremlin,
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<()>,
    },

    // ── Sessions ───────────────────────────────────────────────────────
    CreateSession {
        respond: oneshot::Sender<String>,
    },
    RemoveSession {
        session_id: String,
        respond: oneshot::Sender<bool>,
    },
    SessionTakeGremlin {
        session_id: String,
        id: String,
        respond: oneshot::Sender<Option<Gremlin>>,
    },
    /// Re-insert a finished turn's gremlin if the session still owns it and
    /// the run-map entry is still live. `false` means the gremlin was dropped.
    SessionReinsertGremlin {
        session_id: String,
        id: String,
        gremlin: Gremlin,
        respond: oneshot::Sender<bool>,
    },
    SessionGetCurrentTurn {
        session_id: String,
        respond: oneshot::Sender<Option<TurnSnapshot>>,
    },
    SessionSetCurrentTurn {
        session_id: String,
        turn: CurrentTurn,
        respond: oneshot::Sender<()>,
    },
    /// Clear `current_turn` if it still points at `cancel` (Arc identity).
    SessionClearCurrentTurn {
        session_id: String,
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<bool>,
    },
    /// Cancel a turn: `id` filters by gremlin id, `soft` selects the
    /// recover-partial-then-end mode. Returns the turn's `done` receiver.
    SessionCancelTurn {
        session_id: String,
        id: Option<String>,
        soft: bool,
        respond: oneshot::Sender<Option<watch::Receiver<bool>>>,
    },
    /// Mark `current_turn.interrupted` (soft-interrupt path), guarded by the
    /// turn's cancel token.
    #[allow(dead_code)]
    SessionMarkInterrupted {
        session_id: String,
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<bool>,
    },
    SessionIsClosed {
        session_id: String,
        respond: oneshot::Sender<bool>,
    },
    /// Mark the session closed and cancel the in-flight turn.
    SessionMarkClosed {
        session_id: String,
        respond: oneshot::Sender<()>,
    },
    #[allow(dead_code)]
    SessionContainsChatId {
        session_id: String,
        id: String,
        respond: oneshot::Sender<bool>,
    },
    SessionAddChatId {
        session_id: String,
        id: String,
        respond: oneshot::Sender<()>,
    },
    SessionRemoveChatId {
        session_id: String,
        id: String,
        respond: oneshot::Sender<bool>,
    },
    SessionGetChatIds {
        session_id: String,
        respond: oneshot::Sender<Vec<String>>,
    },
    SessionGetChatGremlinKeys {
        session_id: String,
        respond: oneshot::Sender<Vec<String>>,
    },
    /// Merge the operator's requested skill set into the session's active set.
    /// Returns the resulting active names and the shared `Arc`.
    SessionUpdateSkills {
        session_id: String,
        requested: Vec<String>,
        respond: oneshot::Sender<Option<ActiveSkills>>,
    },
    #[allow(dead_code)]
    SessionGetActiveSkills {
        session_id: String,
        respond: oneshot::Sender<Option<Arc<Mutex<Vec<String>>>>>,
    },
    /// Tear the session down. The reply fires once the drain has finished;
    /// the drain itself runs in a task spawned by the actor (see module docs).
    SessionTeardown {
        session_id: String,
        abort_listener: bool,
        respond: oneshot::Sender<()>,
    },
    /// Re-insert a gremlin and mark `current_turn.interrupted` atomically
    /// (guarded by the turn's cancel token). No TOCTOU window.
    SessionReinsertAndMarkInterrupted {
        session_id: String,
        id: String,
        gremlin: Gremlin,
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<bool>,
    },
    SessionSetListenerTask {
        session_id: String,
        task: JoinHandle<()>,
        respond: oneshot::Sender<()>,
    },
    SessionPushAuxSocket {
        session_id: String,
        task: JoinHandle<()>,
        respond: oneshot::Sender<()>,
    },
    SessionGetAll {
        respond: oneshot::Sender<Vec<String>>,
    },
    /// The model of the session's chat gremlin, live or parked.
    SessionGetChatModel {
        session_id: String,
        respond: oneshot::Sender<Option<String>>,
    },

    // ── Connections ────────────────────────────────────────────────────
    /// One more control/session connection. Sent from `ConnectionGuard::new`.
    ConnectionUp,
    /// One fewer connection; may trigger idle shutdown. Sent from `Drop`.
    ConnectionDown,

    // ── Shutdown ───────────────────────────────────────────────────────
    #[allow(dead_code)]
    Shutdown,

    // ── Id locks ───────────────────────────────────────────────────────
    /// Fetch the per-id lock. The caller acquires it itself (see
    /// [`acquire_id_lock`]) so the actor never blocks on it.
    AcquireIdLock {
        id: String,
        respond: oneshot::Sender<Arc<tokio::sync::Mutex<()>>>,
    },
}

// ---------------------------------------------------------------------------
// Actor state
// ---------------------------------------------------------------------------

/// All supervisor state. Owned by [`run_actor`]; never shared.
pub(crate) struct ActorState {
    /// Live runs — the single authority on gremlin liveness (the old
    /// `RUN_MAP`).
    runs: HashMap<String, RunHandle>,
    /// Client sessions (the old `SESSION_MAP`).
    sessions: HashMap<String, SessionState>,
    /// Open control/session connections (the old `CONNECTION_COUNT`).
    connection_count: usize,
    /// Fires `true` to stop the accept loop.
    shutdown_tx: watch::Sender<bool>,
    /// State root, used to unlink session sockets on teardown.
    state_root: PathBuf,
    /// Per-id async mutexes that serialize `rm`/`resume`/`snapshot` (the old
    /// `ID_LOCKS`).
    id_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Handle on our own mailbox, cloned into spawned run tasks so they can
    /// report completion.
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
}

/// Spawn the actor task and return its mailbox sender.
pub(crate) fn spawn_actor(
    state_root: PathBuf,
    shutdown_tx: watch::Sender<bool>,
) -> mpsc::UnboundedSender<ActorMsg> {
    let (actor_tx, msg_rx) = mpsc::unbounded_channel();
    let task_tx = actor_tx.clone();
    tokio::spawn(async move {
        run_actor(msg_rx, task_tx, state_root, shutdown_tx).await;
    });
    actor_tx
}

/// The actor loop: process one message at a time, forever.
pub(crate) async fn run_actor(
    mut msg_rx: mpsc::UnboundedReceiver<ActorMsg>,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
    state_root: PathBuf,
    shutdown_tx: watch::Sender<bool>,
) {
    let mut state = ActorState {
        runs: HashMap::new(),
        sessions: HashMap::new(),
        connection_count: 0,
        shutdown_tx,
        state_root,
        id_locks: HashMap::new(),
        actor_tx,
    };

    while let Some(msg) = msg_rx.recv().await {
        state.handle_message(msg).await;
    }
}

/// Acquire the per-id lock without blocking the actor.
///
/// The actor only hands out the `Arc`; the (possibly long) wait for the guard
/// happens here, on the caller's task. Waiting inside the actor would deadlock
/// as soon as the current holder needs a reply from it (`rm`/`resume` both
/// issue actor messages while holding the lock).
pub(crate) async fn acquire_id_lock(
    actor_tx: &mpsc::UnboundedSender<ActorMsg>,
    id: &str,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    let (respond, rx) = oneshot::channel();
    actor_tx
        .send(ActorMsg::AcquireIdLock {
            id: id.to_string(),
            respond,
        })
        .ok()?;
    let lock = rx.await.ok()?;
    Some(lock.lock_owned().await)
}

/// Terminal [`RunState`] for a run that left the map.
fn terminal_state(id: &str, status: &str) -> RunState {
    RunState {
        id: id.to_string(),
        status: status.to_string(),
        stage: String::new(),
        started_at: String::new(),
    }
}

impl ActorState {
    // -------------------------------------------------------------------
    // Thin dispatcher — each concern delegates to a private method.
    // -------------------------------------------------------------------

    async fn handle_message(&mut self, msg: ActorMsg) {
        match msg {
            // ── Launch (three near-identical arms share insert_run_entry) ──
            ActorMsg::LaunchRun {
                gremlin,
                stage_label,
                resume_from,
                definition_name,
                notify_shutdown,
                respond,
            } => {
                self.handle_launch_run(
                    gremlin,
                    stage_label,
                    resume_from,
                    definition_name,
                    notify_shutdown,
                    respond,
                )
                .await
            }

            ActorMsg::LaunchChild {
                gremlin,
                relay,
                respond,
            } => self.handle_launch_child(gremlin, relay, respond).await,

            ActorMsg::RegisterChatGremlin {
                session_id,
                id,
                gremlin,
                cancel,
                respond,
            } => {
                self.handle_register_chat_gremlin(session_id, id, gremlin, cancel, respond)
                    .await
            }

            // ── Run lifecycle ───────────────────────────────────────────
            other @ (ActorMsg::RunFinished { .. }
            | ActorMsg::StopRun { .. }
            | ActorMsg::ContainsRun { .. }
            | ActorMsg::GetRunStore { .. }
            | ActorMsg::GetRunInteractive { .. }
            | ActorMsg::GetRunStageStream { .. }
            | ActorMsg::GetRunStageStreamAndFlush { .. }
            | ActorMsg::SetRunInteractiveCmd { .. }
            | ActorMsg::GetRunLogFlush { .. }
            | ActorMsg::GetRunCancel { .. }
            | ActorMsg::ListRunStores { .. }
            | ActorMsg::ListRunIds { .. }
            | ActorMsg::ListRunStoresWithIds { .. }
            | ActorMsg::SetRunCancel { .. }) => self.handle_run_lifecycle(other).await,

            // ── Sessions ───────────────────────────────────────────────
            other @ (ActorMsg::CreateSession { .. }
            | ActorMsg::RemoveSession { .. }
            | ActorMsg::SessionTakeGremlin { .. }
            | ActorMsg::SessionReinsertGremlin { .. }
            | ActorMsg::SessionGetCurrentTurn { .. }
            | ActorMsg::SessionSetCurrentTurn { .. }
            | ActorMsg::SessionClearCurrentTurn { .. }
            | ActorMsg::SessionCancelTurn { .. }
            | ActorMsg::SessionMarkInterrupted { .. }
            | ActorMsg::SessionIsClosed { .. }
            | ActorMsg::SessionMarkClosed { .. }
            | ActorMsg::SessionContainsChatId { .. }
            | ActorMsg::SessionAddChatId { .. }
            | ActorMsg::SessionRemoveChatId { .. }
            | ActorMsg::SessionGetChatIds { .. }
            | ActorMsg::SessionGetChatGremlinKeys { .. }
            | ActorMsg::SessionUpdateSkills { .. }
            | ActorMsg::SessionGetActiveSkills { .. }
            | ActorMsg::SessionTeardown { .. }
            | ActorMsg::SessionReinsertAndMarkInterrupted { .. }
            | ActorMsg::SessionSetListenerTask { .. }
            | ActorMsg::SessionPushAuxSocket { .. }
            | ActorMsg::SessionGetAll { .. }
            | ActorMsg::SessionGetChatModel { .. }) => self.handle_session(other).await,

            // ── Connections ────────────────────────────────────────────
            ActorMsg::ConnectionUp => self.connection_count += 1,
            ActorMsg::ConnectionDown => {
                self.connection_count = self.connection_count.saturating_sub(1);
                if self.connection_count == 0 {
                    self.maybe_shutdown();
                }
            }

            // ── Shutdown ───────────────────────────────────────────────
            ActorMsg::Shutdown => {
                let _ = self.shutdown_tx.send(true);
            }

            // ── Id locks ───────────────────────────────────────────────
            ActorMsg::AcquireIdLock { id, respond } => {
                let lock = self
                    .id_locks
                    .entry(id)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone();
                let _ = respond.send(lock);
            }
        }
    }

    // -------------------------------------------------------------------
    // Launch: handle_launch_run, handle_launch_child, handle_register_chat_gremlin
    // -------------------------------------------------------------------

    async fn handle_launch_run(
        &mut self,
        mut gremlin: Gremlin,
        stage_label: String,
        resume_from: Option<String>,
        definition_name: String,
        notify_shutdown: bool,
        respond: oneshot::Sender<String>,
    ) {
        let id = gremlin.id.to_string();
        // Seed the actor mailbox so the launched gremlin's nested stages
        // (agent interactive swap, parallel children) can reach the run map
        // without a global.
        gremlin.runtime_config.actor_tx = Some(self.actor_tx.clone());
        let (stage_stream_broadcast, interactive, log_flush, log_sub) =
            setup_gremlin_channels(&mut gremlin, None);
        gremlin.runtime_config.stream_events =
            Some(StreamEventSender::Broadcast(interactive.evt_tx.clone()));

        let cancel = CancelToken::new();
        gremlin.cancel_token = Some(Arc::clone(&cancel));
        let store = gremlin.state.store_handle();
        let aborted = Arc::new(AtomicBool::new(false));
        // The oneshot barrier keeps the task from running — and finishing —
        // before the run-map entry exists. The actor inserting sequentially
        // already closes the race; the barrier is kept until the spawned-task
        // plumbing goes away.
        let (go_tx, go_rx) = oneshot::channel::<()>();

        let task = {
            let actor_tx = self.actor_tx.clone();
            let id = id.clone();
            let aborted = Arc::clone(&aborted);
            tokio::spawn(async move {
                let _ = go_rx.await;
                let (result, _gremlin) = run_gremlin_task(gremlin, resume_from.as_deref()).await;
                if !aborted.load(Ordering::Relaxed) {
                    let _ = actor_tx.send(ActorMsg::RunFinished {
                        id,
                        result,
                        notify_shutdown,
                    });
                }
            })
        };

        self.insert_run_entry(
            id.clone(),
            cancel,
            Some(task),
            aborted,
            stage_stream_broadcast,
            interactive,
            store,
            Some(log_flush),
            Some(log_sub),
            RunState {
                id: id.clone(),
                status: "running".to_string(),
                stage: stage_label.clone(),
                started_at: state::now_stamp(),
            },
        );

        emit_event(EventKind::RunStarted {
            id: id.clone(),
            definition: definition_name,
            stage: stage_label,
        });

        let _ = go_tx.send(());
        let _ = respond.send(id);
    }

    async fn handle_launch_child(
        &mut self,
        mut gremlin: Gremlin,
        relay: Option<LogRelay>,
        respond: oneshot::Sender<LaunchResult>,
    ) {
        let id = gremlin.id.to_string();
        gremlin.runtime_config.actor_tx = Some(self.actor_tx.clone());
        let (stage_stream_broadcast, interactive, log_flush, log_sub) =
            setup_gremlin_channels(&mut gremlin, relay);
        gremlin.runtime_config.stream_events =
            Some(StreamEventSender::Broadcast(interactive.evt_tx.clone()));

        // Children inherit the cancel token their parent installed.
        let cancel = gremlin
            .cancel_token
            .clone()
            .unwrap_or_else(CancelToken::new);
        let store = gremlin.state.store_handle();
        let definition_name = definition_name_from_tree(&store.state_tree());
        let aborted = Arc::new(AtomicBool::new(false));

        let (go_tx, go_rx) = oneshot::channel::<()>();
        let (gremlin_tx, gremlin_rx) = oneshot::channel();
        let task = {
            let actor_tx = self.actor_tx.clone();
            let id = id.clone();
            let aborted = Arc::clone(&aborted);
            tokio::spawn(async move {
                let _ = go_rx.await;
                let (result, gremlin) = run_gremlin_task(gremlin, None).await;
                let _ = gremlin_tx.send(gremlin);
                if !aborted.load(Ordering::Relaxed) {
                    let _ = actor_tx.send(ActorMsg::RunFinished {
                        id,
                        result,
                        notify_shutdown: false,
                    });
                }
            })
        };

        let state_rx = self.insert_run_entry(
            id.clone(),
            cancel,
            Some(task),
            aborted,
            stage_stream_broadcast,
            interactive,
            store,
            Some(log_flush),
            Some(log_sub),
            RunState {
                id: id.clone(),
                status: "running".to_string(),
                stage: "starting".to_string(),
                started_at: state::now_stamp(),
            },
        );

        emit_event(EventKind::RunStarted {
            id: id.clone(),
            definition: definition_name,
            stage: "starting".to_string(),
        });

        let _ = go_tx.send(());
        let _ = respond.send(LaunchResult {
            state_rx,
            gremlin_rx,
        });
    }

    async fn handle_register_chat_gremlin(
        &mut self,
        session_id: String,
        id: String,
        mut gremlin: Gremlin,
        cancel: Arc<CancelToken>,
        respond: oneshot::Sender<()>,
    ) {
        // Refuse registration when the session no longer exists — a concurrent
        // `SessionTeardown` may have already removed it. Without a session the
        // gremlin would be immediately orphaned (a live run-map entry with no
        // owner and no task to stop it), which also blocks `maybe_shutdown`
        // idle-exit forever.
        if !self.sessions.contains_key(&session_id) {
            let _ = respond.send(());
            return;
        }

        gremlin.runtime_config.actor_tx = Some(self.actor_tx.clone());
        let (stage_stream_broadcast, interactive, log_flush, log_sub) =
            setup_gremlin_channels(&mut gremlin, None);
        gremlin.runtime_config.stream_events =
            Some(StreamEventSender::Broadcast(interactive.evt_tx.clone()));

        self.insert_run_entry(
            id.clone(),
            cancel,
            // Chat gremlins have no spawned run task — their turn runs in
            // `handle_chat`.
            None,
            Arc::new(AtomicBool::new(false)),
            stage_stream_broadcast,
            interactive,
            gremlin.state.store_handle(),
            Some(log_flush),
            Some(log_sub),
            RunState {
                id: id.clone(),
                status: "running".to_string(),
                stage: "chat".to_string(),
                started_at: state::now_stamp(),
            },
        );

        // Park the gremlin in its session.
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.chat_ids.insert(id.clone());
            session.chat_gremlins.insert(id.clone(), gremlin);
        }

        emit_event(EventKind::RunStarted {
            id,
            definition: "chat".to_string(),
            stage: "chat".to_string(),
        });

        let _ = respond.send(());
    }

    // -------------------------------------------------------------------
    // Shared run-map insertion — canonical home for the insertion protocol.
    // -------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn insert_run_entry(
        &mut self,
        id: String,
        cancel: Arc<CancelToken>,
        task: Option<tokio::task::JoinHandle<()>>,
        aborted: Arc<AtomicBool>,
        stage_stream_broadcast: tokio::sync::broadcast::Sender<String>,
        interactive: InteractiveHandle,
        store: StoreHandle,
        log_flush: Option<LogFlushHandle>,
        log_sub: Option<Subscriber<EventKind>>,
        initial_state: RunState,
    ) -> watch::Receiver<RunState> {
        let (state_tx, state_rx) = watch::channel(initial_state);
        self.runs.insert(
            id,
            RunHandle {
                cancel,
                task,
                aborted,
                state_tx,
                stage_stream_broadcast,
                interactive,
                store,
                log_flush,
                log_sub,
            },
        );
        state_rx
    }

    // -------------------------------------------------------------------
    // Run lifecycle (RunFinished through SetRunCancel)
    // -------------------------------------------------------------------

    async fn handle_run_lifecycle(&mut self, msg: ActorMsg) {
        match msg {
            ActorMsg::RunFinished {
                id,
                result,
                notify_shutdown,
            } => {
                // Remove before publishing: if the entry is already gone, a
                // stop won the race and already published the terminal state.
                let Some(handle) = self.runs.remove(&id) else {
                    return;
                };

                let _ = handle.state_tx.send(terminal_state(
                    &id,
                    if result == 0 { "done" } else { "stopped" },
                ));

                if result == 0 {
                    emit_event(EventKind::RunCompleted {
                        id: id.clone(),
                        exit_code: 0,
                    });
                } else {
                    emit_event(EventKind::RunFailed {
                        id: id.clone(),
                        exit_code: result,
                        error: None,
                    });
                }

                self.release_log_sub(handle.log_sub);

                if notify_shutdown {
                    self.maybe_shutdown();
                }
            }

            ActorMsg::StopRun { id, respond } => {
                let Some(handle) = self.runs.remove(&id) else {
                    let _ = respond.send(None);
                    return;
                };

                handle.cancel.cancel();
                if let Some(task) = &handle.task {
                    task.abort();
                }
                handle.aborted.store(true, Ordering::Relaxed);

                let _ = handle.state_tx.send(terminal_state(&id, "stopped"));

                // The caller (`stop_child` / `handle_stop`) handles log-sub
                // cleanup after it writes terminal state and reaps the
                // backend. The actor only publishes the terminal event; the
                // subscription is the caller's responsibility to unsubscribe.
                let _ = respond.send(Some((handle.state_tx.clone(), handle.log_sub)));
            }

            ActorMsg::ContainsRun { id, respond } => {
                let _ = respond.send(self.runs.contains_key(&id));
            }

            ActorMsg::GetRunStore { id, respond } => {
                let _ = respond.send(self.runs.get(&id).map(|handle| handle.store.clone()));
            }

            ActorMsg::GetRunInteractive { id, respond } => {
                let _ = respond.send(self.runs.get(&id).map(|handle| handle.interactive.clone()));
            }

            ActorMsg::GetRunStageStream { id, respond } => {
                let _ = respond.send(
                    self.runs
                        .get(&id)
                        .map(|handle| handle.stage_stream_broadcast.clone()),
                );
            }

            ActorMsg::GetRunStageStreamAndFlush { id, respond } => {
                let result = self
                    .runs
                    .get(&id)
                    .map(|handle| {
                        (
                            Some(handle.stage_stream_broadcast.clone()),
                            handle.log_flush.clone(),
                        )
                    })
                    .unwrap_or((None, None));
                let _ = respond.send(result);
            }

            ActorMsg::SetRunInteractiveCmd {
                id,
                cmd_tx,
                respond,
            } => {
                let done = match self.runs.get_mut(&id) {
                    Some(handle) => {
                        handle.interactive.cmd_tx = cmd_tx;
                        true
                    }
                    None => false,
                };
                let _ = respond.send(done);
            }

            ActorMsg::GetRunLogFlush { id, respond } => {
                let _ = respond.send(
                    self.runs
                        .get(&id)
                        .and_then(|handle| handle.log_flush.clone()),
                );
            }

            ActorMsg::GetRunCancel { id, respond } => {
                let _ = respond.send(self.runs.get(&id).map(|handle| Arc::clone(&handle.cancel)));
            }

            ActorMsg::ListRunStores { respond } => {
                let stores = self
                    .runs
                    .iter()
                    .map(|(id, handle)| (id.clone(), handle.store.clone()))
                    .collect();
                let _ = respond.send(stores);
            }

            ActorMsg::ListRunIds { respond } => {
                let _ = respond.send(self.runs.keys().cloned().collect());
            }

            ActorMsg::ListRunStoresWithIds { respond } => {
                let ids: Vec<String> = self.runs.keys().cloned().collect();
                let stores = self
                    .runs
                    .iter()
                    .map(|(id, handle)| (id.clone(), handle.store.clone()))
                    .collect();
                let _ = respond.send((ids, stores));
            }

            ActorMsg::SetRunCancel {
                id,
                cancel,
                respond,
            } => {
                let found = match self.runs.get_mut(&id) {
                    Some(handle) => {
                        handle.cancel = cancel;
                        true
                    }
                    None => false,
                };
                let _ = respond.send(found);
            }

            // Routing error: the dispatcher should only route run-lifecycle
            // variants here. Log and no-op so a future enum addition doesn't
            // crash the single-threaded actor.
            other => {
                log::error!(
                    "handle_run_lifecycle: unexpected variant {:?}",
                    std::mem::discriminant(&other)
                );
            }
        }
    }

    // -------------------------------------------------------------------
    // Sessions (all 20+ session variants)
    // -------------------------------------------------------------------

    async fn handle_session(&mut self, msg: ActorMsg) {
        match msg {
            ActorMsg::CreateSession { respond } => {
                let session_id = loop {
                    let candidate = format!("sess-{}", state::token_hex(8));
                    if !self.sessions.contains_key(&candidate) {
                        self.sessions
                            .insert(candidate.clone(), SessionState::new(candidate.clone()));
                        break candidate;
                    }
                };
                let _ = respond.send(session_id);
            }

            ActorMsg::RemoveSession {
                session_id,
                respond,
            } => {
                let _ = respond.send(self.sessions.remove(&session_id).is_some());
            }

            ActorMsg::SessionTakeGremlin {
                session_id,
                id,
                respond,
            } => {
                let gremlin = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(|session| session.chat_gremlins.remove(&id));
                let _ = respond.send(gremlin);
            }

            ActorMsg::SessionReinsertGremlin {
                session_id,
                id,
                gremlin,
                respond,
            } => {
                let reinserted = match self.sessions.get_mut(&session_id) {
                    Some(session)
                        if session.chat_ids.contains(&id) && self.runs.contains_key(&id) =>
                    {
                        session.chat_gremlins.insert(id, gremlin);
                        true
                    }
                    _ => false,
                };
                let _ = respond.send(reinserted);
            }

            ActorMsg::SessionGetCurrentTurn {
                session_id,
                respond,
            } => {
                let turn = self
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.current_turn.as_ref())
                    .map(|cur| TurnSnapshot {
                        id: cur.id.clone(),
                        turn: cur.turn.clone(),
                        cancel: Arc::clone(&cur.cancel),
                        done: cur.done.clone(),
                        interrupted: cur.interrupted,
                    });
                let _ = respond.send(turn);
            }

            ActorMsg::SessionSetCurrentTurn {
                session_id,
                turn,
                respond,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.current_turn = Some(turn);
                }
                let _ = respond.send(());
            }

            ActorMsg::SessionClearCurrentTurn {
                session_id,
                cancel,
                respond,
            } => {
                let mut cleared = false;
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    let matches = session
                        .current_turn
                        .as_ref()
                        .is_some_and(|cur| Arc::ptr_eq(&cur.cancel, &cancel));
                    if matches {
                        session.current_turn = None;
                        cleared = true;
                    }
                }
                let _ = respond.send(cleared);
            }

            ActorMsg::SessionCancelTurn {
                session_id,
                id,
                soft,
                respond,
            } => {
                let done = self
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.cancel_turn(id.as_deref(), soft));
                let _ = respond.send(done);
            }

            ActorMsg::SessionMarkInterrupted {
                session_id,
                cancel,
                respond,
            } => {
                let mut marked = false;
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    if let Some(cur) = session.current_turn.as_mut() {
                        if Arc::ptr_eq(&cur.cancel, &cancel) {
                            cur.interrupted = true;
                            marked = true;
                        }
                    }
                }
                let _ = respond.send(marked);
            }

            ActorMsg::SessionIsClosed {
                session_id,
                respond,
            } => {
                let closed = self
                    .sessions
                    .get(&session_id)
                    .map(|session| session.closed)
                    .unwrap_or(true);
                let _ = respond.send(closed);
            }

            ActorMsg::SessionMarkClosed {
                session_id,
                respond,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.closed = true;
                    if let Some(cur) = &session.current_turn {
                        cur.cancel.cancel();
                    }
                }
                let _ = respond.send(());
            }

            ActorMsg::SessionContainsChatId {
                session_id,
                id,
                respond,
            } => {
                let contains = self
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.chat_ids.contains(&id));
                let _ = respond.send(contains);
            }

            ActorMsg::SessionAddChatId {
                session_id,
                id,
                respond,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.chat_ids.insert(id);
                }
                let _ = respond.send(());
            }

            ActorMsg::SessionRemoveChatId {
                session_id,
                id,
                respond,
            } => {
                let removed = self
                    .sessions
                    .get_mut(&session_id)
                    .is_some_and(|session| session.chat_ids.remove(&id));
                let _ = respond.send(removed);
            }

            ActorMsg::SessionGetChatIds {
                session_id,
                respond,
            } => {
                let ids = self
                    .sessions
                    .get(&session_id)
                    .map(|session| session.chat_ids.iter().cloned().collect())
                    .unwrap_or_default();
                let _ = respond.send(ids);
            }

            ActorMsg::SessionGetChatGremlinKeys {
                session_id,
                respond,
            } => {
                let keys = self
                    .sessions
                    .get(&session_id)
                    .map(|session| session.chat_gremlins.keys().cloned().collect())
                    .unwrap_or_default();
                let _ = respond.send(keys);
            }

            ActorMsg::SessionUpdateSkills {
                session_id,
                requested,
                respond,
            } => {
                let result = self.sessions.get_mut(&session_id).map(|session| {
                    let skills = Arc::clone(&session.active_skills);
                    let prev_operator = std::mem::take(&mut session.operator_skills);
                    let mut guard = skills.lock().unwrap();
                    for name in &requested {
                        if !guard.contains(name) {
                            guard.push(name.clone());
                        }
                    }
                    guard.retain(|name| requested.contains(name) || !prev_operator.contains(name));
                    let names = guard.clone();
                    drop(guard);
                    session.operator_skills = requested;
                    (names, skills)
                });
                let _ = respond.send(result);
            }

            ActorMsg::SessionGetActiveSkills {
                session_id,
                respond,
            } => {
                let skills = self
                    .sessions
                    .get(&session_id)
                    .map(|session| Arc::clone(&session.active_skills));
                let _ = respond.send(skills);
            }

            ActorMsg::SessionTeardown {
                session_id,
                abort_listener,
                respond,
            } => {
                let Some(job) = self.take_teardown(&session_id, abort_listener) else {
                    let _ = respond.send(());
                    return;
                };
                let actor_tx = self.actor_tx.clone();
                tokio::spawn(async move {
                    complete_teardown(job, actor_tx).await;
                    let _ = respond.send(());
                });
            }

            ActorMsg::SessionReinsertAndMarkInterrupted {
                session_id,
                id,
                gremlin,
                cancel,
                respond,
            } => {
                let mut done = false;
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    if session.chat_ids.contains(&id) && self.runs.contains_key(&id) {
                        session.chat_gremlins.insert(id.clone(), gremlin);
                        if let Some(cur) = session.current_turn.as_mut() {
                            if Arc::ptr_eq(&cur.cancel, &cancel) {
                                cur.interrupted = true;
                            }
                        }
                        done = true;
                    }
                }
                let _ = respond.send(done);
            }

            ActorMsg::SessionSetListenerTask {
                session_id,
                task,
                respond,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.listener_task = Some(task);
                } else {
                    task.abort();
                }
                let _ = respond.send(());
            }

            ActorMsg::SessionPushAuxSocket {
                session_id,
                task,
                respond,
            } => {
                match self.sessions.get_mut(&session_id) {
                    Some(session) if !session.closed => session.aux_sockets.push(task),
                    _ => task.abort(),
                }
                let _ = respond.send(());
            }

            ActorMsg::SessionGetAll { respond } => {
                let _ = respond.send(self.sessions.keys().cloned().collect());
            }

            ActorMsg::SessionGetChatModel {
                session_id,
                respond,
            } => {
                let model = self.session_chat_model(&session_id);
                let _ = respond.send(model);
            }

            // Routing error: the dispatcher should only route session
            // variants here. Log and no-op so a future enum addition doesn't
            // crash the single-threaded actor.
            other => {
                log::error!(
                    "handle_session: unexpected variant {:?}",
                    std::mem::discriminant(&other)
                );
            }
        }
    }

    // -------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------

    /// Unsubscribe a finished run's log-writer subscription so its log task
    /// drains and releases the writer + state store.
    fn release_log_sub(&self, sub: Option<Subscriber<EventKind>>) {
        if let Some(sub) = sub {
            init_bus().unsubscribe(&sub);
        }
    }

    /// Signal idle shutdown when nothing is left to do.
    fn maybe_shutdown(&self) {
        if self.runs.is_empty() && self.connection_count == 0 {
            let _ = self.shutdown_tx.send(true);
        }
    }

    /// Resolve the model in use by a session's chat gremlin — parked first,
    /// then live in the run map (an in-flight turn parks its gremlin outside
    /// the session).
    fn session_chat_model(&self, session_id: &str) -> Option<String> {
        let session = self.sessions.get(session_id)?;
        let id = session
            .chat_gremlins
            .keys()
            .next()
            .cloned()
            .or_else(|| session.current_turn.as_ref().map(|turn| turn.id.clone()))?;
        let parked = session
            .chat_gremlins
            .get(&id)
            .map(|gremlin| gremlin.state.read_str("model"))
            .filter(|model| !model.is_empty());
        parked.or_else(|| {
            self.runs
                .get(&id)
                .map(|handle| handle.store.read_str("model"))
                .filter(|model| !model.is_empty())
        })
    }

    /// Take everything a teardown needs out of the session map. Idempotent:
    /// a session already torn down yields `None`.
    fn take_teardown(&mut self, session_id: &str, abort_listener: bool) -> Option<TeardownJob> {
        if self.sessions.get(session_id)?.torn_down {
            return None;
        }
        let mut session = self.sessions.remove(session_id)?;
        session.torn_down = true;
        session.closed = true;

        // Cancel (and get a handle on) the in-flight turn before dropping
        // the session — the turn task signals `done` through its own `Arc`.
        let turn_done = session.cancel_turn(None, false);

        let chat_ids: Vec<String> = session.chat_ids.drain().collect();
        let chat_gremlins = std::mem::take(&mut session.chat_gremlins);
        let aux_sockets = std::mem::take(&mut session.aux_sockets);
        let mut listener_task = session.listener_task.take();
        if !abort_listener {
            // The listener is already exiting on its own; just drop the handle.
            listener_task = None;
        }

        Some(TeardownJob {
            session_id: session_id.to_string(),
            state_root: self.state_root.clone(),
            listener_task,
            aux_sockets,
            chat_ids,
            chat_gremlins,
            turn_done,
        })
    }
}

/// Finish a teardown outside the actor loop.
///
/// Order matters and mirrors the old `teardown_session_impl`: stop the
/// listener, abort aux sockets (so their connection guards run), wait for the
/// cancelled turn to relinquish its gremlin, then reap the chat gremlins and
/// drop the parked ones.
async fn complete_teardown(job: TeardownJob, actor_tx: mpsc::UnboundedSender<ActorMsg>) {
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
        // Stop first so the actor removes the run-map entry and publishes
        // the terminal state; the reap below must not race a still-live handle.
        let (respond, rx) = oneshot::channel();
        if actor_tx
            .send(ActorMsg::StopRun {
                id: id.clone(),
                respond,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
        super::lifecycle::terminate_and_reap(id);
    }

    // Drop parked gremlins only after their run-map entries are gone.
    drop(job.chat_gremlins);

    // The session is gone from the map; the socket is dead weight now.
    socket::unlink_session_socket(&job.state_root, &job.session_id);
    log::debug!("actor: session {} torn down", job.session_id);
}
