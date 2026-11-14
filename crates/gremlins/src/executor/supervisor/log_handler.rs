//! Streaming log handler (`handle_log` op).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::mpsc;

use crate::config;
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket;
use crate::executor::state::{self, FileSystemStateStore};
use crate::magic_bus::Subscriber;

use super::actor_client::Client;
use super::event::{init_bus, EventKind};
use super::ops::resolve_store_for_id;
use super::{error_response, SharedWriter};

pub(crate) async fn handle_log(
    request: &Value,
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    if validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    // Resolve the log source: for live gremlins (including ephemeral) the
    // historical log is read through the store's `log` blob; for persisted
    // gremlins open the store via StateData (matching handle_debug); only
    // when that fails fall back to direct filesystem read.
    enum LogSource {
        Store(Arc<dyn state::StateStore + Send + Sync>),
        Path(PathBuf),
    }

    let (source, unknown) = match resolve_store_for_id(id, actor_tx).await {
        Ok(store) => (LogSource::Store(store), false),
        Err(_) => {
            let sd = FileSystemStateStore::open(id)
                .map(|store| store.root_dir().to_path_buf())
                .unwrap_or_else(|_| config::state_root().join(format!("{id}-0001")));
            if sd.is_dir() && sd.join("state.json").is_file() {
                (LogSource::Path(sd), false)
            } else {
                (LogSource::Path(PathBuf::new()), true)
            }
        }
    };

    if unknown {
        let resp = error_response(&format!("unknown gremlin {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let follow = request
        .get("follow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Only subscribe for live runs (still in the run map). A finished run
    // has no more events; subscribing would wait forever on a sender that
    // already emitted its terminal event. The combined message atomically
    // checks the run map and subscribes inside the actor, closing the TOCTOU
    // window that existed when these were two separate messages.
    let (log_sub, mut log_rx) = if follow {
        let client = Client::new(actor_tx.clone());
        let (flush, sub_rx) =
            if let Some((flush, sub, rx)) = client.get_run_log_flush_and_subscribe(id).await {
                (Some(flush), (Some(sub), Some(rx)))
            } else {
                (None, (None, None))
            };
        // Request the log writer flush queued events to disk so
        // pre-subscription lines are visible in the blob we're about
        // to read.
        if let Some(flush) = &flush {
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let _ = flush.send(ack_tx);
            let _ = ack_rx.await;
        }
        sub_rx
    } else {
        (None, None)
    };

    // Clean up the bus subscription on exit so we don't leak a registry
    // entry; the log_writer subscription and any other per-run subs are
    // separate.
    let unsubscribe = |sub: &Option<Subscriber<EventKind>>| {
        if let Some(ref sub) = sub {
            init_bus().unsubscribe(sub);
        }
    };

    // 1. Send historical lines from the log.
    let content = match source {
        LogSource::Store(store) => match store.open("log", state::BlobMode::Read) {
            Ok(mut blob) => tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let mut s = String::new();
                blob.read_to_string(&mut s).ok();
                s
            })
            .await
            .unwrap_or_default(),
            Err(_) => String::new(),
        },
        LogSource::Path(state_dir) => tokio::fs::read_to_string(state_dir.join("log"))
            .await
            .unwrap_or_default(),
    };

    let mut offset: u64 = 0;
    // Forward each line as a JSON log_line event.
    macro_rules! log_line {
        ($line:expr) => {
            let payload = serde_json::json!({"type": "log_line", "line": $line, "offset": offset});
            if writer.write_json_line(&payload).await.is_err() { unsubscribe(&log_sub); return; }
            offset += 1;
        };
    }
    // Build a per-line deduplication fingerprint set from the replayed
    // content. Replayed lines may carry a `[timestamp] message` prefix;
    // we strip the stamp and normalize to just the message body so the
    // same message arriving live won't be duplicated. Multiline messages
    // are split just like the log writer and live path.
    let mut replayed: HashSet<String> = HashSet::new();
    for line in content.lines() {
        // Strip timestamp prefix if present so we dedupe against the
        // raw message the bus delivers.
        let body = line.split_once("] ").map(|(_, msg)| msg).unwrap_or(line);
        replayed.insert(body.to_string());
        log_line!(line);
    }

    // 2. If follow, drain duplicates (events that arrived during file replay),
    //    then stream new lines. Monitor the read half for client disconnect.
    if let Some(ref mut log_rx) = log_rx {
        // Drain queued events, re-emitting only those whose per-line message
        // was not already replayed from the file. Bus events arrive at emit
        // time, so they may precede the log writer's blob flush — discarding
        // them blindly would lose lines that were never persisted.
        while let Ok(dispatch) = log_rx.try_recv() {
            if let EventKind::LogLine { id: line_id, line } = dispatch.kind() {
                // Ancestor matching delivers child LogLine events (e.g.
                // `parent:child` → `r:parent`). Drop those — the LogRelay
                // pathway emits prefixed copies with the parent id set.
                if line_id != id {
                    continue;
                }
                let ts =
                    crate::executor::state::stamp_from_epoch_millis(dispatch.ts().wall_millis());
                for part in line.split('\n') {
                    let stamped = format!("{ts} {part}");
                    if !replayed.contains(part) {
                        log_line!(stamped);
                    }
                }
            }
        }

        loop {
            tokio::select! {
                result = log_rx.recv() => {
                    match result {
                        crate::magic_bus::Recv::Event(dispatch) => {
                            match dispatch.kind() {
                                EventKind::LogLine { id: line_id, line } => {
                                    // Keep only the logged-in run's own lines —
                                    // child lines arrive via ancestor matching
                                    // but are served through the LogRelay pathway
                                    // with a prefix.
                                    if line_id != id {
                                        continue;
                                    }
                                    let ts = crate::executor::state::stamp_from_epoch_millis(
                                        dispatch.ts().wall_millis(),
                                    );
                                    for part in line.split('\n') {
                                        let stamped = format!("{ts} {part}");
                                        log_line!(stamped);
                                    }
                                }
                                // Terminal event: the run is done. Only break
                                // when the terminal event's id matches the requested
                                // run — ancestor matching may deliver child terminal
                                // events (e.g. `parent:child` completing), but the
                                // parent follower must continue for siblings and the
                                // parent's own terminal event.
                                EventKind::RunCompleted { id: ev_id, .. }
                                | EventKind::RunFailed { id: ev_id, .. }
                                | EventKind::RunStopped { id: ev_id }
                                | EventKind::RunRemoved { id: ev_id }
                                    if ev_id == id =>
                                {
                                    // Unregister so no new events arrive.
                                    unsubscribe(&log_sub);
                                    // Drain buffered lines (child relay
                                    // diagnostics emitted after the terminal
                                    // event).
                                    while let Ok(dispatch) = log_rx.try_recv() {
                                        if let EventKind::LogLine { id: line_id, line } =
                                            dispatch.kind()
                                        {
                                            // Post-terminal drain: same
                                            // run-scoped filter as above.
                                            if line_id != id {
                                                continue;
                                            }
                                            let ts = crate::executor::state::stamp_from_epoch_millis(
                                                dispatch.ts().wall_millis(),
                                            );
                                            for part in line.split('\n') {
                                                let stamped = format!("{ts} {part}");
                                                log_line!(stamped);
                                            }
                                        }
                                    }
                                    break;
                                }
                                _ => continue,
                            }
                        }
                        crate::magic_bus::Recv::Lagged(n) => {
                            log_line!(format!("[skipped {n} lines]"));
                        }
                        crate::magic_bus::Recv::Closed => break,
                    }
                }
                _ = socket::read_json_line(&mut reader) => { break; }
            }
        }
    }
    unsubscribe(&log_sub);
}
