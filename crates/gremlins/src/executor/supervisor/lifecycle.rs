//! Child gremlin lifecycle: channel setup, task spawning, log writing, and terminal reap.

use std::sync::Arc;

use crate::clients::client::Client;
use crate::clients::interactive::{InteractiveHandle, InteractiveSessionFactory};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;

use gremlins_magic_bus::EventKind;
use gremlins_magic_bus::{emit_event, init_bus};
use gremlins_magic_bus::{CorrId, ReceiverHalf, SubscriberRegistration};

/// Best-effort relay of a child's log into the parent's event bus.
#[derive(Clone)]
pub(crate) struct LogRelay {
    pub parent_id: String,
    pub prefix: String,
}

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
    gremlins_magic_bus::Subscriber<EventKind>,
) {
    let (sessions, interactive_handle) = if gremlin.state.read_str("kind") == "chat" {
        InteractiveSessionFactory::for_chat()
    } else {
        InteractiveSessionFactory::for_pipeline()
    };
    sessions.set_run_id(gremlin.id.to_string());
    gremlin.interactive_sessions = Some(sessions);

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

    let log_flush = spawn_log_writer(log_rx, store, relay, run_id);
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
    run_id: String,
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
                        gremlins_magic_bus::Recv::Event(dispatch) => {
                            // Use the event's captured timestamp so the persisted
                            // record matches the emit time, not the receive time.
                            let ts =
                                crate::executor::state::stamp_from_epoch_millis(dispatch.ts().wall_millis());
                            let line = match dispatch.kind() {
                                // Skip descendant log lines (e.g. `parent:child`):
                                // the run-scoped subscription matches them via
                                // ancestor matching, but they are re-emitted with
                                // a prefix through the LogRelay pathway. Writing
                                // them here too would double-deliver each line.
                                EventKind::LogLine { id, line } => {
                                    if *id != run_id {
                                        continue;
                                    }
                                    line
                                }
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
                        gremlins_magic_bus::Recv::Lagged(_) => {
                            // Log writer can't meaningfully recover from lag, but
                            // this is Lossless so lag shouldn't occur.
                        }
                        gremlins_magic_bus::Recv::Closed => break,
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

// ── Terminal reap ──────────────────────────────────────────────────────

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
