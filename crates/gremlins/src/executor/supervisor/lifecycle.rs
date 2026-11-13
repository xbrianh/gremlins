//! Child gremlin lifecycle: launch, stop, channel setup, and task spawning.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, watch};

use crate::clients::cancel::CancelToken;
use crate::clients::client::Client;
use crate::clients::interactive::{InteractiveChannels, InteractiveHandle, StreamEventSender};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::magic_bus::{CorrId, ReceiverHalf, SubscriberRegistration};

use super::event::{emit_event, init_bus, EventKind};
use super::state_view::definition_name_from_tree;
use super::{get_run_map, RunHandle, RunState};

/// Set up log + interactive + stage-stream channels on a gremlin. Returns the
/// stage-stream broadcast sender and interactive handle for insertion into the
/// run map.
pub(crate) fn setup_gremlin_channels(
    gremlin: &mut Gremlin,
    relay: Option<LogRelay>,
) -> (
    broadcast::Sender<String>,
    InteractiveHandle,
    tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
) {
    let channels = InteractiveChannels::new();
    let (interactive_handle, interactive_session) = channels.split();
    gremlin.runtime_config.interactive = Some(interactive_handle.clone());
    gremlin.interactive_session = Some(interactive_session);

    // Stage-aware live stream: a dedicated broadcast that carries JSON lines
    // for agent events (reasoning/text/tool) and exec stdout/stderr.
    let (stage_stream_broadcast, _) = broadcast::channel(256);
    gremlin.runtime_config.stage_stream_tx = Some(stage_stream_broadcast.clone());
    crate::executor::stage_stream::spawn_stage_stream_forwarder(
        interactive_handle.evt_tx.subscribe(),
        stage_stream_broadcast.clone(),
    );

    let store = gremlin.state.store_handle();
    let run_id = gremlin.id.to_string();

    // Subscribe to the bus with a run-scoped prefix so the log writer
    // only receives events for this gremlin.
    let (_log_sub, log_rx) = init_bus()
        .subscribe(SubscriberRegistration::lossless(
            CorrId::new().with_run(&run_id),
        ))
        .expect("lossless subscription cannot fail");

    // Stage boundaries are pushed directly into `stage_stream_broadcast` by
    // `send_stage_boundary` (synchronously, before the stage's content). The
    // matching `StageBoundary` event still flows on the bus for socket
    // forwarders and Phase 2, so no per-gremlin forwarding subscription is
    // needed here — adding one would race with content forwarders and would
    // leak a task/mailbox for the daemon's lifetime.

    let log_flush = spawn_log_writer(log_rx, store, relay);
    (stage_stream_broadcast, interactive_handle, log_flush)
}

async fn run_gremlin_task(mut gremlin: Gremlin, resume_from: Option<&str>) -> (i32, Gremlin) {
    let exit_code = match gremlin.run(resume_from).await {
        Ok(ec) => ec,
        Err(e) => {
            log::error!("gremlin {}: {e}", gremlin.id.as_str());
            emit_event(EventKind::LogLine {
                id: gremlin.id.to_string(),
                line: format!("error: {e}"),
            });
            crate::executor::stage_stream::send_stage_boundary(
                &gremlin.runtime_config.stage_stream_tx,
                gremlin.id.as_str(),
                "",
                "other",
            );
            gremlin.state.write_terminal_state(1);
            1
        }
    };
    (exit_code, gremlin)
}

/// Spawn a task that reads log events from the bus and appends to the state
/// blob, returning a channel to request a flush barrier (persist queued events).
fn spawn_log_writer(
    mut log_rx: ReceiverHalf<EventKind>,
    store: Arc<dyn state::StateStore + Send + Sync>,
    relay: Option<LogRelay>,
) -> tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>> {
    let (barrier_tx, mut barrier_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();

    enum WriteCmd {
        Line(String),
        Flush(tokio::sync::oneshot::Sender<()>),
    }

    tokio::spawn(async move {
        let file: Option<Arc<std::sync::Mutex<Box<dyn state::StateBlob>>>> =
            match store.open("log", state::BlobMode::Append) {
                Ok(blob) => Some(Arc::new(std::sync::Mutex::new(blob))),
                Err(e) => {
                    log::error!("log writer: failed to open log blob: {e}");
                    None
                }
            };

        // A single long-lived blocking writer drains a channel of stamped
        // fragments, so the async task never blocks on the synchronous
        // `StateBlob` handle and no blocking-pool job is spawned per line.
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<WriteCmd>();
        let writer = file.map(|file| {
            tokio::task::spawn_blocking(move || {
                while let Some(cmd) = write_rx.blocking_recv() {
                    match cmd {
                        WriteCmd::Line(stamped) => {
                            let mut guard = match file.lock() {
                                Ok(guard) => guard,
                                Err(_) => return,
                            };
                            let _ = guard.write_all(stamped.as_bytes());
                            let _ = guard.write_all(b"\n");
                            let _ = guard.flush();
                        }
                        WriteCmd::Flush(ack) => {
                            let mut guard = match file.lock() {
                                Ok(guard) => guard,
                                Err(_) => return,
                            };
                            let _ = guard.flush();
                            let _ = ack.send(());
                        }
                    }
                }
            })
        });

        loop {
            tokio::select! {
                result = log_rx.recv() => {
                    match result {
                        crate::magic_bus::Recv::Event(dispatch) => {
                            // Use the event's captured timestamp so the persisted
                            // record matches the emit time, not the receive time.
                            let ts =
                                crate::executor::state::stamp_from_epoch_millis(dispatch.ts().wall_millis());
                            let line = match dispatch.kind() {
                                EventKind::LogLine { line, .. } => line,
                                _ => continue,
                            };
                            for part in line.split('\n') {
                                let stamped = format!("{ts} {part}");
                                if writer.is_some() {
                                    let _ = write_tx.send(WriteCmd::Line(stamped.clone()));
                                }
                                if let Some(relay) = &relay {
                                    emit_event(EventKind::LogLine {
                                        id: relay.parent_id.clone(),
                                        line: format!("{}{}", relay.prefix, part),
                                    });
                                }
                            }
                        }
                        crate::magic_bus::Recv::Lagged(_) => {
                            // Log writer can't meaningfully recover from lag, but
                            // this is Lossless so lag shouldn't occur.
                        }
                        crate::magic_bus::Recv::Closed => break,
                    }
                }
                // Flush barrier: send a flush command through the write channel;
                // the blocking writer acknowledges it after all prior writes
                // have been flushed to the blob.
                Some(ack) = barrier_rx.recv() => {
                    if writer.is_some() {
                        let _ = write_tx.send(WriteCmd::Flush(ack));
                    } else {
                        let _ = ack.send(());
                    }
                }
            }
        }
    });
    barrier_tx
}

pub(crate) fn spawn_gremlin_runner(
    mut gremlin: Gremlin,
    id: &str,
    stage_label: &str,
    resume_from: Option<&str>,
    definition_name: &str,
    shutdown_tx: &watch::Sender<bool>,
) {
    let (stage_stream_broadcast, interactive_handle, log_flush) =
        setup_gremlin_channels(&mut gremlin, None);
    gremlin.runtime_config.stream_events = Some(StreamEventSender::Broadcast(
        interactive_handle.evt_tx.clone(),
    ));
    let store = gremlin.state.store_handle();

    let cancel_token = CancelToken::new();
    let (state_tx, _state_rx) = watch::channel(RunState {
        id: id.to_string(),
        status: "running".to_string(),
        stage: stage_label.to_string(),
        started_at: state::now_stamp(),
    });

    let aborted = Arc::new(AtomicBool::new(false));
    gremlin.cancel_token = Some(cancel_token.clone());
    let id_owned = id.to_string();
    let state_tx_clone = state_tx.clone();
    let shutdown_tx_clone = shutdown_tx.clone();
    let resume_from_owned = resume_from.map(String::from);
    let aborted_for_task = aborted.clone();

    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, _gremlin) = run_gremlin_task(gremlin, resume_from_owned.as_deref()).await;
        if !aborted_for_task.load(Ordering::Relaxed) {
            RunHandle::finish(&id_owned, result, &state_tx_clone, Some(&shutdown_tx_clone));
        }
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: Some(join_handle),
        aborted,
        state_tx,
        stage_stream_broadcast,
        interactive: interactive_handle,
        store,
        log_flush: Some(log_flush),
    };
    get_run_map().lock().unwrap().insert(id.to_string(), handle);

    emit_event(EventKind::RunStarted {
        id: id.to_string(),
        definition: definition_name.to_string(),
        stage: stage_label.to_string(),
    });

    let _ = go_tx.send(());
}

// ---------------------------------------------------------------------------
// launch_child / stop_child — public API for the parallel executor
// ---------------------------------------------------------------------------

/// Result of [`launch_child`]: a state watch receiver that fires when the
/// child reaches a terminal state, and a oneshot receiver for the child
/// [`Gremlin`] after its task completes.
pub(crate) struct LaunchResult {
    pub state_rx: watch::Receiver<RunState>,
    pub gremlin_rx: tokio::sync::oneshot::Receiver<Gremlin>,
}

/// Best-effort relay of a child's log into the parent's event bus.
#[derive(Clone)]
pub(crate) struct LogRelay {
    pub parent_id: String,
    pub prefix: String,
}

/// Launch a pre-configured child gremlin into the run map. Synchronous — no `.await`.
pub(crate) fn launch_child(mut gremlin: Gremlin, relay: Option<LogRelay>) -> LaunchResult {
    let id = gremlin.id.to_string();

    let (stage_stream_broadcast, interactive_handle, log_flush) =
        setup_gremlin_channels(&mut gremlin, relay);
    gremlin.runtime_config.stream_events = Some(StreamEventSender::Broadcast(
        interactive_handle.evt_tx.clone(),
    ));
    let store = gremlin.state.store_handle();

    let cancel_token = gremlin
        .cancel_token
        .clone()
        .unwrap_or_else(CancelToken::new);
    let (state_tx, state_rx) = watch::channel(RunState {
        id: id.clone(),
        status: "running".to_string(),
        stage: "starting".to_string(),
        started_at: state::now_stamp(),
    });

    let state_tx_clone = state_tx.clone();

    let aborted = Arc::new(AtomicBool::new(false));
    let aborted_for_task = aborted.clone();

    let id_clone = id.clone();

    // Spawn before inserting into run_map so the JoinHandle is available
    // from the moment the entry exists. A oneshot barrier prevents the
    // task from running (and potentially finishing) before the entry is
    // inserted — no race window in either direction.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel();
    let (gremlin_tx, gremlin_rx) = tokio::sync::oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let _ = go_rx.await;
        let (result, gremlin) = run_gremlin_task(gremlin, None).await;
        // Send the gremlin back to the parallel coordinator so it can
        // keep TempDirs alive during post-processing.
        let _ = gremlin_tx.send(gremlin);
        if !aborted_for_task.load(Ordering::Relaxed) {
            // Don't signal shutdown — the parent gremlin is still running.
            RunHandle::finish(
                &id_clone,
                result,
                &state_tx_clone,
                None::<&watch::Sender<bool>>,
            );
        }
    });

    let handle = RunHandle {
        cancel: cancel_token,
        task: Some(join_handle),
        aborted: aborted.clone(),
        state_tx,
        stage_stream_broadcast,
        interactive: interactive_handle,
        store: store.clone(),
        log_flush: Some(log_flush),
    };
    // Insert into RUN_MAP *after* spawning so the JoinHandle is present
    // from the moment the entry exists.
    get_run_map().lock().unwrap().insert(id.clone(), handle);

    // Emit RunStarted so subscribers see the child before its terminal
    // event (finish already emits RunCompleted/RunFailed).
    let definition_name = definition_name_from_tree(&store.state_tree());
    emit_event(EventKind::RunStarted {
        id: id.clone(),
        definition: definition_name,
        stage: "starting".to_string(),
    });

    // Now the entry is visible — let the task proceed.
    let _ = go_tx.send(());

    LaunchResult {
        state_rx,
        gremlin_rx,
    }
}

/// Stop a child gremlin by aborting its task and cleaning up.
///
/// A no-op when the child is not in the run map (already finished or never
/// launched).
pub(crate) async fn stop_child(id: &str) {
    let run_map = get_run_map();
    let state_tx = {
        let mut map = run_map.lock().unwrap();
        match map.get(id) {
            Some(handle) => {
                handle.cancel.cancel();
                if let Some(ref task) = handle.task {
                    task.abort();
                }
                handle.aborted.store(true, Ordering::Relaxed);
                // Clone what we need before removing the entry.
                let state_tx = handle.state_tx.clone();
                // Remove from run_map and broadcast terminal state while we
                // still hold the lock — no race window for a duplicate finish.
                map.remove(id);
                Some(state_tx)
            }
            None => None,
        }
    };
    if let Some(state_tx) = state_tx {
        // Don't signal shutdown — the parent gremlin is still running.
        let _ = state_tx.send(RunState {
            id: id.to_string(),
            status: "stopped".to_string(),
            stage: String::new(),
            started_at: String::new(),
        });
        // Write terminal state to disk so the aborted child isn't
        // reported as orphan and has a recorded exit_code.
        if let Ok(sd) = state::StateData::open(id) {
            sd.write_terminal_state(1);
        }
        // Reap backend resources that the aborted task would have
        // reaped in Gremlin::finish.
        reap_client_for(id);
    }
}

pub(crate) fn reap_client_for(id: &str) {
    let spec = state::StateData::open(id)
        .map(|sd| sd.read_str("model"))
        .unwrap_or_default();
    if spec.is_empty() {
        return;
    }
    match Client::parse(&spec) {
        Ok(client) => client.reap_all(id),
        Err(e) => log::warn!("reap_client_for {id}: failed to parse model spec {spec:?}: {e}"),
    }
}

pub(crate) fn terminate_and_reap(id: &str) {
    if let Ok(sd) = state::StateData::open(id) {
        sd.write_terminal_state(1);
    }
    reap_client_for(id);
    emit_event(EventKind::RunStopped { id: id.to_string() });
}
