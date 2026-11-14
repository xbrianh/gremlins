//! Child gremlin lifecycle: launch, stop, channel setup, and task spawning.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::clients::client::Client;
use crate::clients::interactive::{InteractiveChannels, InteractiveHandle};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::magic_bus::{CorrId, ReceiverHalf, SubscriberRegistration};

use super::event::{emit_event, init_bus, EventKind};
use super::RunState;

/// Set up log + interactive + stage-stream channels on a gremlin. Returns the
/// interactive handle, log flush channel, and the log-writer bus subscriber
/// (so the caller can unsubscribe when the run finishes).
#[allow(clippy::type_complexity)]
pub(crate) fn setup_gremlin_channels(
    gremlin: &mut Gremlin,
    relay: Option<LogRelay>,
) -> (
    InteractiveHandle,
    tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
    crate::magic_bus::Subscriber<EventKind>,
) {
    let channels = InteractiveChannels::new();
    let (interactive_handle, interactive_session) = channels.split();
    gremlin.runtime_config.interactive = Some(interactive_handle.clone());
    gremlin.interactive_session = Some(interactive_session);

    let store = gremlin.state.store_handle();
    let run_id = gremlin.id.to_string();

    // Subscribe to the bus with a run-scoped prefix so the log writer
    // only receives events for this gremlin.
    let (log_sub, log_rx) = init_bus()
        .subscribe(SubscriberRegistration::lossless(
            CorrId::new().with_run(&run_id),
        ))
        .expect("lossless subscription cannot fail");

    // Stage boundaries flow on the bus via send_stage_boundary; the
    // stage_stream proxy subscriber picks them up.

    let log_flush = spawn_log_writer(log_rx, store, relay);
    (interactive_handle, log_flush, log_sub)
}

pub(crate) async fn run_gremlin_task(
    mut gremlin: Gremlin,
    resume_from: Option<&str>,
) -> (i32, Gremlin) {
    let exit_code = match gremlin.run(resume_from).await {
        Ok(ec) => ec,
        Err(e) => {
            log::error!("gremlin {}: {e}", gremlin.id.as_str());
            emit_event(EventKind::LogLine {
                id: gremlin.id.to_string(),
                line: format!("error: {e}"),
            });
            crate::executor::stage_stream::send_stage_boundary(gremlin.id.as_str(), "", "other");
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
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    gremlin: Gremlin,
    stage_label: &str,
    resume_from: Option<&str>,
    definition_name: &str,
    notify_shutdown: bool,
) {
    let (respond, _rx) = tokio::sync::oneshot::channel();
    actor_tx
        .send(super::actor::ActorMsg::LaunchRun {
            gremlin,
            stage_label: stage_label.to_string(),
            resume_from: resume_from.map(String::from),
            definition_name: definition_name.to_string(),
            notify_shutdown,
            respond,
        })
        .expect("actor alive");
    // Fire-and-forget: the actor sends the id back but no caller awaits it.
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

/// Launch a pre-configured child gremlin into the run map.
///
/// The actor owns channel setup, the run-map insert, the spawned task and the
/// `RunStarted` event; this just sends `LaunchChild` and awaits the reply.
pub(crate) async fn launch_child(
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    gremlin: Gremlin,
    relay: Option<LogRelay>,
) -> LaunchResult {
    let (respond, rx) = tokio::sync::oneshot::channel();
    actor_tx
        .send(super::actor::ActorMsg::LaunchChild {
            gremlin,
            relay,
            respond,
        })
        .expect("actor alive");
    rx.await.expect("actor died")
}

/// Stop a child gremlin by aborting its task and cleaning up.
///
/// A no-op when the child is not in the run map (already finished or never
/// launched).
pub(crate) async fn stop_child(actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>, id: &str) {
    let (respond, rx) = tokio::sync::oneshot::channel();
    let _ = actor_tx.send(super::actor::ActorMsg::StopRun {
        id: id.to_string(),
        respond,
    });
    // The actor already removed the entry, published the terminal state, and
    // released the log subscription; `Some(..)` means the id was live and the
    // follow-up work is ours.
    if let Ok(Some(state_tx)) = rx.await {
        let _ = state_tx.send(RunState {
            id: id.to_string(),
            status: "stopped".to_string(),
            stage: String::new(),
            started_at: String::new(),
        });
        // Write terminal state to disk so the aborted child isn't reported as
        // orphan and has a recorded exit_code.
        if let Ok(sd) = state::StateData::open(id) {
            sd.write_terminal_state(1);
        }
        // Reap backend resources that the aborted task would have reaped in
        // Gremlin::finish.
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
