//! Streaming log handler (`handle_log` op).
//!
//! The outer `handle_log` orchestrates three focused stages:
//! 1. `resolve_log_source` — determines the log source (store vs path).
//! 2. `replay_history` — replays a fixed persisted snapshot.
//! 3. `drain_and_follow` — skips the snapshot's bus overlap and streams new events.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;

use crate::config;
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::state::{self, FileSystemStateStore};

use crate::executor::socket;
use gremlins_magic_bus::connection::SharedWriter;
use gremlins_magic_bus::Subscriber;
use gremlins_magic_bus::{init_bus, EventKind};

use super::error_response;
use super::ops::resolve_store_for_id;
use super::run_registry::{LogSnapshot, RunRegistry};

// ── Log source resolution ───────────────────────────────────────────────

enum LogSource {
    Store(Arc<dyn state::StateStore + Send + Sync>),
    Path(PathBuf),
}

/// Resolve the log source: for live gremlins (including ephemeral) the
/// historical log is read through the store's `log` blob; for persisted
/// gremlins open the store via StateData; only when that fails fall back to
/// direct filesystem read.
async fn resolve_log_source(
    id: &str,
    registry: &Arc<RunRegistry>,
) -> (LogSource, /* unknown: */ bool) {
    match resolve_store_for_id(id, registry).await {
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
    }
}

// ── History replay ──────────────────────────────────────────────────────

/// A writer that pushes log_line JSON and an unsubscribe guard, threaded
/// through the history/follow helpers so the orchestrator doesn't carry them.
struct LogOutput<'a> {
    writer: &'a SharedWriter,
    log_sub: &'a Option<Subscriber<EventKind>>,
}

impl LogOutput<'_> {
    /// Send a `{"type":"log_line", "line":…, "offset":…}` JSON payload.
    /// Returns `true` on success, `false` when the write fails (client gone).
    async fn send_line(&self, line: &str, offset: &mut u64) -> bool {
        let payload = serde_json::json!({"type": "log_line", "line": line, "offset": *offset});
        if self.writer.write_json_line(&payload).await.is_err() {
            unsubscribe(self.log_sub);
            return false;
        }
        *offset += 1;
        true
    }
}

fn unsubscribe(sub: &Option<Subscriber<EventKind>>) {
    if let Some(ref sub) = sub {
        init_bus().unsubscribe(sub);
    }
}

async fn read_history(source: &LogSource) -> String {
    match source {
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
    }
}

async fn replay_history(content: &str, out: &LogOutput<'_>) -> Option<u64> {
    let mut offset: u64 = 0;
    for line in content.lines() {
        if !out.send_line(line, &mut offset).await {
            // Client disconnected — signal the orchestrator to bail.
            return None;
        }
    }
    Some(offset)
}

// ── Drain + follow ──────────────────────────────────────────────────────

/// Skip only events included in the writer's atomic snapshot, then follow.
async fn drain_and_follow(
    id: &str,
    mut log_rx: gremlins_magic_bus::ReceiverHalf<EventKind>,
    out: &LogOutput<'_>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    through_sequence: u64,
    offset: &mut u64,
) {
    loop {
        tokio::select! {
            result = log_rx.recv() => {
                match result {
                    gremlins_magic_bus::Recv::Event(dispatch) => {
                        match dispatch.kind() {
                            EventKind::LogLine { id: line_id, line } => {
                                if line_id != id || (dispatch.sequence() != 0 && dispatch.sequence() <= through_sequence) {
                                    continue;
                                }
                                let ts = crate::executor::state::stamp_from_epoch_millis(
                                    dispatch.ts().wall_millis(),
                                );
                                for part in line.split('\n') {
                                    let stamped = format!("{ts} {part}");
                                    if !out.send_line(&stamped, offset).await {
                                        return;
                                    }
                                }
                            }
                            EventKind::RunCompleted { id: ev_id, .. }
                            | EventKind::RunFailed { id: ev_id, .. }
                            | EventKind::RunStopped { id: ev_id }
                            | EventKind::RunRemoved { id: ev_id }
                                if ev_id == id =>
                            {
                                // Drain buffered post-terminal lines.
                                unsubscribe(out.log_sub);
                                while let Ok(dispatch) = log_rx.try_recv() {
                                    if let EventKind::LogLine { id: line_id, line } =
                                        dispatch.kind()
                                    {
                                        if line_id != id || (dispatch.sequence() != 0 && dispatch.sequence() <= through_sequence) {
                                            continue;
                                        }
                                        let ts = crate::executor::state::stamp_from_epoch_millis(
                                            dispatch.ts().wall_millis(),
                                        );
                                        for part in line.split('\n') {
                                            let stamped = format!("{ts} {part}");
                                            if !out.send_line(&stamped, offset).await {
                                                return;
                                            }
                                        }
                                    }
                                }
                                return;
                            }
                            _ => continue,
                        }
                    }
                    gremlins_magic_bus::Recv::Lagged(n) => {
                        if !out.send_line(
                            &format!("[skipped {n} lines]"),
                            offset,
                        ).await {
                            return;
                        }
                    }
                    gremlins_magic_bus::Recv::Closed => return,
                }
            }
            _ = socket::read_json_line(&mut reader) => return,
        }
    }
}

// ── Orchestrator ─────────────────────────────────────────────────────────

pub(crate) async fn handle_log(
    request: &Value,
    registry: &Arc<RunRegistry>,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
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

    // 1. Resolve the log source.
    let (source, unknown) = resolve_log_source(id, registry).await;
    if unknown {
        let resp = error_response(&format!("unknown gremlin {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let follow = request
        .get("follow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 2. Subscribe for live runs and flush pre-subscription events.
    let (log_sub, log_rx, snapshot) = if follow {
        let (flush, sub_rx) =
            if let Some((flush, sub, rx)) = registry.get_run_log_flush_and_subscribe(id).await {
                (Some(flush), Some((sub, rx)))
            } else {
                (None, None)
            };

        let snapshot = if let Some(flush) = &flush {
            LogSnapshot::request(flush).await
        } else {
            None
        };

        match sub_rx {
            Some((sub, rx)) => (Some(sub), Some(rx), snapshot),
            None => (None, None, None),
        }
    } else {
        (None, None, registry.get_run_log_snapshot(id).await)
    };

    let out = LogOutput {
        writer,
        log_sub: &log_sub,
    };

    // 3. Replay persisted history. Bail on client disconnect.
    let LogSnapshot {
        content,
        through_sequence,
    } = match snapshot {
        Some(snapshot) => snapshot,
        None => LogSnapshot {
            content: read_history(&source).await,
            through_sequence: 0,
        },
    };
    let Some(mut offset) = replay_history(&content, &out).await else {
        return;
    };

    // 4. Drain and follow live events.
    if let Some(log_rx) = log_rx {
        drain_and_follow(id, log_rx, &out, reader, through_sequence, &mut offset).await;
    }

    unsubscribe(&log_sub);
}

#[cfg(test)]
#[path = "log_handler_tests.rs"]
mod tests;
