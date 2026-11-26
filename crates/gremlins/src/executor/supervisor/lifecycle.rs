//! Child gremlin lifecycle: channel setup, task spawning, log writing, and terminal reap.

use std::sync::Arc;

use futures::FutureExt;

use crate::clients::client::Client;
use crate::clients::interactive::{InteractiveHandle, InteractiveSessionFactory};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;

use super::run_registry::{LogFlushHandle, LogSnapshot, LogSnapshotRequest};
use gremlins_magic_bus::EventKind;
use gremlins_magic_bus::{emit_event, init_bus};
use gremlins_magic_bus::{CorrId, ReceiverHalf, SubscriberRegistration};

/// Best-effort relay of a child's log into the parent's event bus.
#[derive(Clone)]
pub(crate) struct LogRelay {
    pub parent_id: String,
    pub prefix: String,
}

/// Set up log and interactive channels on a gremlin. Returns the
/// interactive handle, log flush channel, and the log-writer bus subscriber
/// (so the caller can unsubscribe when the run finishes).
#[allow(clippy::type_complexity)]
pub(crate) fn setup_gremlin_channels(
    gremlin: &mut Gremlin,
    relay: Option<LogRelay>,
) -> (
    InteractiveHandle,
    LogFlushHandle,
    gremlins_magic_bus::Subscriber<EventKind>,
) {
    let (sessions, interactive_handle) = if gremlin.state.read_str("kind") == "chat" {
        InteractiveSessionFactory::for_chat()
    } else {
        InteractiveSessionFactory::for_pipeline()
    };
    interactive_handle.set_input_registry(gremlin.execution_inputs.clone());
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

    let log_flush = spawn_log_writer(log_rx, store, relay, run_id);
    (interactive_handle, log_flush, log_sub)
}

pub(crate) async fn run_gremlin_task(
    mut gremlin: Gremlin,
    resume_from: Option<&str>,
) -> (i32, Gremlin) {
    // Catch unwinds while retaining the gremlin and its accounting collector.
    let result = std::panic::AssertUnwindSafe(gremlin.run(resume_from))
        .catch_unwind()
        .await;
    let mut exit_code = match result {
        Ok(Ok(ec)) => ec,
        Ok(Err(error)) => {
            record_run_error(
                gremlin.id.as_str(),
                &gremlin.state.store_handle(),
                &error.to_string(),
            );
            1
        }
        Err(_) => {
            record_run_error(
                gremlin.id.as_str(),
                &gremlin.state.store_handle(),
                "run task panicked",
            );
            1
        }
    };
    if let Err(error) = gremlin.flush_accounting().await {
        record_run_error(
            gremlin.id.as_str(),
            &gremlin.state.store_handle(),
            &format!("accounting flush failed: {error}"),
        );
        exit_code = 1;
    }
    if exit_code != 0 {
        gremlin.state.write_terminal_state(exit_code);
    }
    (exit_code, gremlin)
}

/// Persist and publish failures before the terminal state is exposed.
pub(super) fn record_run_error(id: &str, store: &super::run_registry::StoreHandle, detail: &str) {
    store.record_stage_error("other", detail);
    log::error!("gremlin {id}: {detail}");
    emit_event(EventKind::LogLine {
        id: id.to_string(),
        line: format!("error: {detail}"),
    });
}

/// Spawn a task that reads log events from the bus and appends to the state
/// blob, returning a channel to request a flush barrier (persist queued events).
fn spawn_log_writer(
    mut log_rx: ReceiverHalf<EventKind>,
    store: Arc<dyn state::StateStore + Send + Sync>,
    relay: Option<LogRelay>,
    run_id: String,
) -> LogFlushHandle {
    let (barrier_tx, mut barrier_rx) = tokio::sync::mpsc::unbounded_channel::<LogSnapshotRequest>();

    enum WriteCmd {
        Event(String, u64),
        Snapshot(LogSnapshotRequest),
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
        // whole events, so the async task never blocks on the synchronous
        // `StateBlob` handle and no blocking-pool job is spawned per line.
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<WriteCmd>();
        let writer = file.map(|file| {
            tokio::task::spawn_blocking(move || {
                let mut through_sequence = 0;
                while let Some(cmd) = write_rx.blocking_recv() {
                    match cmd {
                        WriteCmd::Event(stamped, sequence) => {
                            let mut guard = match file.lock() {
                                Ok(guard) => guard,
                                Err(_) => return,
                            };
                            if guard.write_all(stamped.as_bytes()).is_err()
                                || guard.flush().is_err()
                            {
                                return;
                            }
                            through_sequence = sequence;
                        }
                        WriteCmd::Snapshot(request) => {
                            use std::io::Read;
                            let Ok(mut blob) = store.open("log", state::BlobMode::Read) else {
                                continue;
                            };
                            let content = match request.max_lines {
                                Some(0) => Ok(String::new()),
                                Some(max_lines) => {
                                    crate::executor::blob_io::read_tail_lines(&mut *blob, max_lines)
                                        .map(|lines| {
                                            if lines.is_empty() {
                                                String::new()
                                            } else {
                                                format!("{}\n", lines.join("\n"))
                                            }
                                        })
                                }
                                None => {
                                    let mut content = String::new();
                                    blob.read_to_string(&mut content).map(|_| content)
                                }
                            };
                            if let Ok(content) = content {
                                let _ = request.ack.send(LogSnapshot {
                                    content,
                                    // Partial history cannot cover earlier bus events.
                                    through_sequence: if request.max_lines.is_some() {
                                        0
                                    } else {
                                        through_sequence
                                    },
                                });
                            }
                        }
                    }
                }
            })
        });

        loop {
            tokio::select! {
                biased;
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
                            let mut stamped = String::new();
                            for part in line.split('\n') {
                                use std::fmt::Write;
                                let _ = writeln!(stamped, "{ts} {part}");
                                if let Some(relay) = &relay {
                                    emit_event(EventKind::LogLine {
                                        id: relay.parent_id.clone(),
                                        line: format!("{}{}", relay.prefix, part),
                                    });
                                }
                            }
                            if writer.is_some() {
                                let _ = write_tx.send(WriteCmd::Event(stamped, dispatch.sequence()));
                            }
                        }
                        gremlins_magic_bus::Recv::Lagged(_) => {
                            // Log writer can't meaningfully recover from lag, but
                            // this is Lossless so lag shouldn't occur.
                        }
                        gremlins_magic_bus::Recv::Closed => break,
                    }
                }
                // Snapshot reads are ordered with writes, so content and watermark
                // describe the same cut even as producers keep emitting.
                Some(ack) = barrier_rx.recv() => {
                    if writer.is_some() {
                        let _ = write_tx.send(WriteCmd::Snapshot(ack));
                    } else {
                        drop(ack);
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
