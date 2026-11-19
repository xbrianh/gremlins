//! Lock-protected run registry + atomic connection counter.
//!
//! Replaces the single-threaded actor (`actor.rs`). All run-map, connection,
//! id-lock, and chat-id state lives in a shared `RunRegistry` behind
//! `Arc<tokio::sync::RwLock<..>>` plus an `AtomicUsize` connection counter.
//!
//! Operations that spawn tasks (launch) take `&Arc<Self>` so they can clone the
//! Arc into spawned tasks. Read-only ops take `&self`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch, RwLock};

use serde_json::{Map, Value};

use crate::clients::cancel::CancelToken;
use crate::clients::interactive::InteractiveHandle;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::state;
use crate::executor::str_field;

use super::lifecycle::reap_client_for;
use super::lifecycle::LogRelay;
use super::lifecycle::{run_gremlin_task, setup_gremlin_channels};
use super::pending_scope::{PendingScopeGuard, PendingScopes};
use gremlins_magic_bus::{emit_event, init_bus, subscribe_run};
use gremlins_magic_bus::{EventKind, ReceiverHalf, Subscriber};

// ── RunHandle / RunState ────────────────────────────────────────────────

pub(crate) struct RunHandle {
    pub cancel: Arc<CancelToken>,
    pub task: Option<tokio::task::JoinHandle<()>>,
    pub aborted: Arc<AtomicBool>,
    #[allow(dead_code)]
    pub state_tx: watch::Sender<RunState>,
    /// Interactive handle (supervisor → agent loop).
    pub interactive: InteractiveHandle,
    /// State store handle — stored so handle_status / handle_info / handle_ls
    /// can read state directly for live gremlins whose state may be in a
    /// TempDir (ephemeral runs) rather than under config::state_root().
    pub store: Arc<dyn state::StateStore + Send + Sync>,
    /// Flush barrier for the log writer: a follower that wants to read
    /// persisted history can request a flush and wait for the ack before
    /// opening the log blob, so that pre-subscription events are visible.
    pub log_flush: Option<tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>>,
    /// Bus subscriber for the run's lossless log-writer subscription.
    /// Held so finish and stop paths can unregister it, preventing the run's
    /// log task, blocking writer, and state store from living until daemon exit.
    pub log_sub: Option<gremlins_magic_bus::Subscriber<EventKind>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunState {
    pub id: String,
    pub status: String,
    pub stage: String,
    pub started_at: String,
}

// ── Re-exports ──────────────────────────────────────────────────────────

/// The state store handle a live run exposes to the query ops.
pub(crate) type StoreHandle = Arc<dyn state::StateStore + Send + Sync>;

/// Flush barrier for a run's log writer.
pub(crate) type LogFlushHandle = mpsc::UnboundedSender<oneshot::Sender<()>>;

/// What `stop_run` hands back when the id was live: the run's state
/// broadcaster so the caller can finish `terminate_and_reap`.
pub(crate) type StoppedRun = watch::Sender<RunState>;

/// Result of `get_run_log_flush_and_subscribe`: flush handle, bus
/// subscriber, and receiver for log follow.
pub(crate) type FlushAndSubscribe = (
    LogFlushHandle,
    Subscriber<EventKind>,
    ReceiverHalf<EventKind>,
);

// ── LaunchResult ──────────────────────────────────────────────────────

/// Result of [`launch_child`]: a state watch receiver that fires when the
/// child reaches a terminal state, and a oneshot receiver for the child
/// [`Gremlin`] after its task completes.
pub(crate) struct LaunchResult {
    pub state_rx: watch::Receiver<RunState>,
    pub gremlin_rx: oneshot::Receiver<Gremlin>,
}

// ── RunRegistry ─────────────────────────────────────────────────────────

/// Shared registry owning the run map, connection counter, per-id locks, and
/// chat-id sequence.
pub(crate) struct RunRegistry {
    /// Live runs (the old `RUN_MAP`). Guarded by a `RwLock`.
    runs: RwLock<HashMap<String, RunHandle>>,
    /// Open control/session connections (the old `CONNECTION_COUNT`).
    connection_count: AtomicUsize,
    /// Fires `true` to stop the accept loop.
    shutdown_tx: watch::Sender<bool>,
    /// Per-id async mutexes that serialize `rm`/`resume`/`snapshot`.
    id_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Monotonic counter for `chat-{seq:04x}` ids.
    chat_seq: RwLock<usize>,
    /// Live pending-output scopes (in memory only, never serialized).
    pending_scopes: PendingScopes,
}

/// Pure helper shared with `state_view`: extract a human-readable definition
/// name from a state tree.
pub(crate) fn definition_name_from_tree(tree: &Map<String, Value>) -> String {
    tree.get("definition_path")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            std::path::Path::new(s)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .unwrap_or_else(|| str_field(tree, "kind"))
}

impl RunRegistry {
    /// Create a new registry.
    pub(crate) fn new(shutdown_tx: watch::Sender<bool>) -> Self {
        Self {
            runs: RwLock::new(HashMap::new()),
            connection_count: AtomicUsize::new(0),
            shutdown_tx,
            id_locks: RwLock::new(HashMap::new()),
            chat_seq: RwLock::new(0),
            pending_scopes: PendingScopes::default(),
        }
    }

    /// Open a pending-output scope for one producer invocation of `run_id`.
    /// `checkout_root` and `outputs` must come from executor-side preparation,
    /// never from a socket request. Destinations are resolved paths beneath
    /// that checkout (not URI-derived paths). Keep the guard with the producer
    /// invocation; dropping it on any exit, including cancellation, revokes it.
    pub(crate) fn open_pending_scope(
        &self,
        run_id: &str,
        checkout_root: std::path::PathBuf,
        outputs: impl IntoIterator<Item = (String, std::path::PathBuf)>,
    ) -> std::io::Result<PendingScopeGuard> {
        self.pending_scopes.open(run_id, checkout_root, outputs)
    }

    /// Missing URIs among a live scope's declared outputs.
    pub(crate) fn missing_in_scope(
        &self,
        run_id: &str,
        token: &str,
        uris: &[String],
    ) -> Result<Vec<String>, String> {
        self.pending_scopes.missing(run_id, token, uris)
    }

    // ── Connection counter ──────────────────────────────────────────────

    /// Test-only: acquire the runs write lock. Exists so the idle-shutdown
    /// invariant test can simulate write contention.
    #[cfg(test)]
    pub(crate) async fn lock_runs_for_test(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, HashMap<String, RunHandle>> {
        self.runs.write().await
    }

    /// Increment the connection count (called from `ConnectionGuard`).
    pub(crate) fn connection_up(&self) {
        self.connection_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the connection count; may trigger idle shutdown.
    pub(crate) fn connection_down(self: &Arc<Self>) {
        let prev = self.connection_count.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            self.check_idle();
        }
    }

    /// Check for idle shutdown: fire when connection count is zero and run map
    /// is empty. When the runs lock is temporarily unavailable (held by a
    /// concurrent write), a fallback task is spawned to retry so one
    /// contended check cannot strand a live-but-idle daemon.
    pub(crate) fn check_idle(self: &Arc<Self>) {
        if self.connection_count.load(Ordering::Relaxed) == 0 {
            if let Ok(runs) = self.runs.try_read() {
                if runs.is_empty() {
                    let _ = self.shutdown_tx.send(true);
                }
                return;
            }
            // Write lock held; spawn a retry that awaits the read lock.
            let this = Arc::clone(self);
            tokio::spawn(async move {
                if this.connection_count.load(Ordering::Relaxed) != 0 {
                    return;
                }
                if this.runs.read().await.is_empty() {
                    let _ = this.shutdown_tx.send(true);
                }
            });
        }
    }

    // ── Run map — read ops ──────────────────────────────────────────────

    pub(crate) async fn contains_run(&self, id: &str) -> bool {
        self.runs.read().await.contains_key(id)
    }

    pub(crate) async fn get_run_store(&self, id: &str) -> Option<StoreHandle> {
        self.runs
            .read()
            .await
            .get(id)
            .map(|handle| handle.store.clone())
    }

    pub(crate) async fn get_run_interactive(
        &self,
        id: &str,
    ) -> Option<(InteractiveHandle, StoreHandle, watch::Receiver<RunState>)> {
        self.runs.read().await.get(id).map(|run| {
            (
                run.interactive.clone(),
                run.store.clone(),
                run.state_tx.subscribe(),
            )
        })
    }

    /// Return the current execution nonce for a run, or `None` if the run
    /// is not live.
    pub(crate) async fn get_run_nonce(&self, id: &str) -> Option<u64> {
        self.runs
            .read()
            .await
            .get(id)
            .map(|handle| handle.interactive.target().nonce())
    }

    /// Combined liveness check + bus subscribe under one lock guard.
    pub(crate) async fn get_run_log_flush_and_subscribe(
        &self,
        id: &str,
    ) -> Option<FlushAndSubscribe> {
        self.runs.read().await.get(id).and_then(|handle| {
            handle.log_flush.clone().map(|flush| {
                let (sub, rx) = subscribe_run(id);
                (flush, sub, rx)
            })
        })
    }

    /// Swap a run's cancel token. Returns `true` if the id was live.
    pub(crate) async fn set_run_cancel(&self, id: &str, cancel: Arc<CancelToken>) -> bool {
        let mut runs = self.runs.write().await;
        match runs.get_mut(id) {
            Some(handle) => {
                handle.cancel = cancel;
                true
            }
            None => false,
        }
    }

    /// List live run ids and their stores atomically.
    pub(crate) async fn list_run_stores_with_ids(
        &self,
    ) -> (Vec<String>, Vec<(String, StoreHandle)>) {
        let runs = self.runs.read().await;
        let ids: Vec<String> = runs.keys().cloned().collect();
        let stores: Vec<(String, StoreHandle)> = runs
            .iter()
            .map(|(id, handle)| (id.clone(), handle.store.clone()))
            .collect();
        (ids, stores)
    }

    // ── Id locks ────────────────────────────────────────────────────────

    /// Acquire the per-id lock. The registry lock is released before the caller
    /// awaits the mutex guard, so no deadlock.
    pub(crate) async fn acquire_id_lock(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.id_locks.write().await;
            locks
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    // ── Chat ids ────────────────────────────────────────────────────────

    /// Reserve a `chat-{seq:04x}` id not present in the live run map.
    pub(crate) async fn reserve_chat_id(&self) -> String {
        loop {
            let candidate = {
                let mut seq = self.chat_seq.write().await;
                *seq += 1;
                format!("chat-{:04x}", *seq)
            };
            let exists = self.runs.read().await.contains_key(&candidate);
            if !exists && validate_gremlin_id(&candidate).is_ok() {
                return candidate;
            }
        }
    }

    // ── Stop ────────────────────────────────────────────────────────────

    /// Stop a run: remove from map, cancel/abort task, publish terminal state,
    /// release log subscription. Returns `None` when the id was not live.
    pub(crate) async fn stop_run(&self, id: &str) -> Option<StoppedRun> {
        let mut runs = self.runs.write().await;
        let handle = runs.remove(id)?;

        handle.cancel.cancel();
        if let Some(task) = &handle.task {
            task.abort();
        }
        handle.aborted.store(true, Ordering::Relaxed);

        let _ = handle.state_tx.send(terminal_state(id, "stopped"));

        release_log_sub(&handle);

        Some(handle.state_tx.clone())
    }

    /// Stop a child run and run the full reap path: terminal state, disk write,
    /// and backend resource cleanup. A no-op when the id was not live.
    pub(crate) async fn stop_and_reap(self: &Arc<Self>, id: &str) {
        if let Some(state_tx) = self.stop_run(id).await {
            let _ = state_tx.send(RunState {
                id: id.to_string(),
                status: "stopped".to_string(),
                stage: String::new(),
                started_at: String::new(),
            });
            if let Ok(sd) = crate::executor::state::StateData::open(id) {
                sd.write_terminal_state(1);
            }
            reap_client_for(id);
        }
    }

    // ── Launch (require &Arc<Self> so we can clone into spawned tasks) ──

    /// Start a fresh top-level run.
    pub(crate) async fn launch_run(
        self: &Arc<Self>,
        mut gremlin: Gremlin,
        stage_label: String,
        resume_from: Option<String>,
        definition_name: String,
        notify_shutdown: bool,
    ) -> String {
        let id = gremlin.id.to_string();
        let common = launch_common(self, &mut gremlin, None);
        let cancel = common.cancel.clone().unwrap_or_else(CancelToken::new);
        gremlin.cancel_token = Some(Arc::clone(&cancel));

        let (go_tx, go_rx) = oneshot::channel::<()>();
        let task = spawn_run_task(
            Arc::clone(self),
            id.clone(),
            Arc::clone(&common.aborted),
            gremlin,
            resume_from,
            notify_shutdown,
            go_rx,
            None,
        );

        let initial = RunState {
            id: id.clone(),
            status: "running".to_string(),
            stage: stage_label.clone(),
            started_at: state::now_stamp(),
        };
        self.insert_run_entry(
            &id,
            cancel,
            Some(task),
            common.aborted,
            common.interactive,
            common.store,
            common.log_flush,
            common.log_sub,
            initial,
        )
        .await;

        emit_event(EventKind::RunStarted {
            id: id.clone(),
            definition: definition_name,
            stage: stage_label,
        });

        let _ = go_tx.send(());
        id
    }

    /// Launch a child gremlin for the parallel executor.
    pub(crate) async fn launch_child(
        self: &Arc<Self>,
        mut gremlin: Gremlin,
        relay: Option<LogRelay>,
    ) -> LaunchResult {
        let id = gremlin.id.to_string();
        let common = launch_common(self, &mut gremlin, relay);
        let cancel = common.cancel.clone().unwrap_or_else(CancelToken::new);
        let definition_name = definition_name_from_tree(&common.store.state_tree());

        let (go_tx, go_rx) = oneshot::channel::<()>();
        let (gremlin_tx, gremlin_rx) = oneshot::channel();

        let task = spawn_run_task(
            Arc::clone(self),
            id.clone(),
            Arc::clone(&common.aborted),
            gremlin,
            None,
            /* notify_shutdown */ false,
            go_rx,
            Some(gremlin_tx),
        );

        let initial = RunState {
            id: id.clone(),
            status: "running".to_string(),
            stage: "starting".to_string(),
            started_at: state::now_stamp(),
        };
        let state_rx = self
            .insert_run_entry(
                &id,
                cancel,
                Some(task),
                common.aborted,
                common.interactive,
                common.store,
                common.log_flush,
                common.log_sub,
                initial,
            )
            .await;

        emit_event(EventKind::RunStarted {
            id: id.clone(),
            definition: definition_name,
            stage: "starting".to_string(),
        });

        let _ = go_tx.send(());
        LaunchResult {
            state_rx,
            gremlin_rx,
        }
    }

    /// Register a chat gremlin's run-map entry without spawning a task.
    pub(crate) async fn register_chat_gremlin(
        self: &Arc<Self>,
        id: String,
        mut gremlin: Gremlin,
        cancel: Arc<CancelToken>,
    ) -> Gremlin {
        let common = launch_common(self, &mut gremlin, None);

        {
            let mut runs = self.runs.write().await;
            if let Some(old) = runs.remove(&id) {
                release_log_sub(&old);
            }
            insert_run_entry_inner(
                &mut runs,
                id.clone(),
                cancel,
                None,
                common.aborted,
                common.interactive,
                common.store,
                common.log_flush,
                common.log_sub,
                RunState {
                    id: id.clone(),
                    status: "running".to_string(),
                    stage: "chat".to_string(),
                    started_at: state::now_stamp(),
                },
            );
        }

        emit_event(EventKind::RunStarted {
            id,
            definition: "chat".to_string(),
            stage: "chat".to_string(),
        });

        gremlin
    }

    // ── Run completion (self-cleanup, called from spawned run tasks) ────

    /// Handle a completed run: remove the map entry, publish the terminal state,
    /// emit the event, release the log subscription, and check idle shutdown.
    pub(super) async fn handle_run_finished(
        self: &Arc<Self>,
        id: String,
        result: i32,
        notify_shutdown: bool,
    ) {
        let mut runs = self.runs.write().await;
        let Some(handle) = runs.remove(&id) else {
            // A stop won the race; it already published the terminal state.
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

        release_log_sub(&handle);

        if notify_shutdown {
            drop(runs);
            self.check_idle();
        }
    }

    /// Fire-and-forget launch, used by the control-plane ops that don't need
    /// the id back (it's the gremlin's own id).
    pub(crate) async fn launch_run_detached(
        self: &Arc<Self>,
        gremlin: Gremlin,
        stage_label: &str,
        resume_from: Option<&str>,
        definition_name: &str,
        notify_shutdown: bool,
    ) {
        let _ = self
            .launch_run(
                gremlin,
                stage_label.to_string(),
                resume_from.map(String::from),
                definition_name.to_string(),
                notify_shutdown,
            )
            .await;
    }

    // ── Private helpers ─────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    async fn insert_run_entry(
        &self,
        id: &str,
        cancel: Arc<CancelToken>,
        task: Option<tokio::task::JoinHandle<()>>,
        aborted: Arc<AtomicBool>,
        interactive: InteractiveHandle,
        store: StoreHandle,
        log_flush: Option<LogFlushHandle>,
        log_sub: Option<Subscriber<EventKind>>,
        initial_state: RunState,
    ) -> watch::Receiver<RunState> {
        let mut runs = self.runs.write().await;
        insert_run_entry_inner(
            &mut runs,
            id.to_string(),
            cancel,
            task,
            aborted,
            interactive,
            store,
            log_flush,
            log_sub,
            initial_state,
        )
    }
}

/// Shared setup extracted from the three launch methods
/// (`launch_run`, `launch_child`, `register_chat_gremlin`).
struct LaunchCommon {
    cancel: Option<Arc<CancelToken>>,
    aborted: Arc<AtomicBool>,
    interactive: InteractiveHandle,
    store: StoreHandle,
    log_flush: Option<LogFlushHandle>,
    log_sub: Option<Subscriber<EventKind>>,
}

fn launch_common(
    registry: &Arc<RunRegistry>,
    gremlin: &mut Gremlin,
    relay: Option<LogRelay>,
) -> LaunchCommon {
    gremlin.runtime_config.registry = Some(Arc::clone(registry));
    let (interactive, log_flush, log_sub) = setup_gremlin_channels(gremlin, relay);
    let store = gremlin.state.store_handle();
    let cancel = gremlin.cancel_token.clone();
    let aborted = Arc::new(AtomicBool::new(false));
    LaunchCommon {
        cancel,
        aborted,
        interactive,
        store,
        log_flush: Some(log_flush),
        log_sub: Some(log_sub),
    }
}

/// Spawn the task that owns a run's `Gremlin`, drives it to completion, and
/// reports the outcome back to the registry.
///
/// Extracted from `launch_run` / `launch_child` into a plain (non-async)
/// function on purpose: the task body awaits [`run_gremlin_task`], which can
/// transitively re-enter the registry through the parallel executor's
/// `launch_child`. Inlining the `tokio::spawn` inside an `async fn` makes that
/// recursion part of the enclosing opaque future's type, which rustc rejects
/// as a cycle (E0391) and cannot prove `Send`. Building the future in a
/// separate non-async frame keeps the cycle out of the registry's own
/// signature.
#[allow(clippy::too_many_arguments)]
fn spawn_run_task(
    registry: Arc<RunRegistry>,
    id: String,
    aborted: Arc<AtomicBool>,
    gremlin: Gremlin,
    resume_from: Option<String>,
    notify_shutdown: bool,
    go_rx: oneshot::Receiver<()>,
    gremlin_tx: Option<oneshot::Sender<Gremlin>>,
) -> tokio::task::JoinHandle<()> {
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(async move {
            let _ = go_rx.await;
            let (result, gremlin) = run_gremlin_task(gremlin, resume_from.as_deref()).await;
            if let Some(tx) = gremlin_tx {
                let _ = tx.send(gremlin);
            }
            if !aborted.load(Ordering::Relaxed) {
                registry
                    .handle_run_finished(id, result, notify_shutdown)
                    .await;
            }
        });
    tokio::spawn(fut)
}

/// Terminal `RunState` for a run that left the map.
fn terminal_state(id: &str, status: &str) -> RunState {
    RunState {
        id: id.to_string(),
        status: status.to_string(),
        stage: String::new(),
        started_at: String::new(),
    }
}

/// Shared run-map insertion.
#[allow(clippy::too_many_arguments)]
fn insert_run_entry_inner(
    runs: &mut HashMap<String, RunHandle>,
    id: String,
    cancel: Arc<CancelToken>,
    task: Option<tokio::task::JoinHandle<()>>,
    aborted: Arc<AtomicBool>,
    interactive: InteractiveHandle,
    store: StoreHandle,
    log_flush: Option<LogFlushHandle>,
    log_sub: Option<Subscriber<EventKind>>,
    initial_state: RunState,
) -> watch::Receiver<RunState> {
    let (state_tx, state_rx) = watch::channel(initial_state);
    runs.insert(
        id,
        RunHandle {
            cancel,
            task,
            aborted,
            state_tx,
            interactive,
            store,
            log_flush,
            log_sub,
        },
    );
    state_rx
}

fn release_log_sub(handle: &RunHandle) {
    if let Some(ref sub) = handle.log_sub {
        init_bus().unsubscribe(sub);
    }
}
