//! Store-blob I/O helpers.
//!
//! Tail reads and line-level utilities for reading persisted run blobs.
//! Used by the `read_blob` session op and (indirectly) by the TUI replay path.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::executor::state;

/// Read the last `max_lines` lines of a store blob without loading the whole
/// file into memory. Reads backward in fixed-size chunks and trims to the
/// requested tail before decoding.
#[allow(dead_code)] // Retained for callers that do not need a handoff watermark.
pub(crate) async fn read_store_blob_tail(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    name: &str,
    max_lines: usize,
) -> Vec<String> {
    read_store_blob_tail_with_snapshot(store, name, max_lines, |read| (read(), None))
        .await
        .0
}

/// Wrap the actual disk read in a synchronous handoff cut inside the blocking
/// task. The wrapper also runs for empty or missing blobs.
/// A failed blocking task returns no watermark rather than claiming a cut.
pub(crate) async fn read_store_blob_tail_with_snapshot<F>(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    name: &str,
    max_lines: usize,
    snapshot: F,
) -> (Vec<String>, Option<u64>)
where
    F: FnOnce(&mut dyn FnMut() -> Vec<String>) -> (Vec<String>, Option<u64>) + Send + 'static,
{
    let store = Arc::clone(store);
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        snapshot(&mut || {
            if max_lines == 0 || !store.exists(&name) {
                Vec::new()
            } else {
                match store.open(&name, state::BlobMode::Read) {
                    Ok(mut blob) => read_tail_lines(&mut *blob, max_lines).unwrap_or_default(),
                    Err(_) => Vec::new(),
                }
            }
        })
    })
    .await
    .unwrap_or_default()
}

/// Raw logs are the replay source, including runner preamble and footer.
/// Legacy sidecars are ignored wholesale: their process-local sequences cannot
/// identify output across restarts, and they may omit output after write errors.
/// No coverage is asserted; callers must use the incomplete replay fallback.
pub(crate) async fn read_exec_tail(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    name: &str,
    max_lines: usize,
) -> (Vec<String>, Option<u64>, Option<Vec<u64>>) {
    let lines = read_store_blob_tail(store, name, max_lines).await;
    (lines, None, None)
}

pub(crate) fn read_tail_lines<R: Read + Seek + ?Sized>(
    blob: &mut R,
    max_lines: usize,
) -> std::io::Result<Vec<String>> {
    let len = blob.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(Vec::new());
    }

    const CHUNK: usize = 8192;
    let mut pos = len;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newline_count = 0usize;

    // Walk backward until we have seen one more newline than the requested
    // number of lines, so the earliest (partial) line can be trimmed.
    while pos > 0 && newline_count <= max_lines {
        let want = CHUNK.min(pos as usize);
        let start = pos - want as u64;
        blob.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; want];
        blob.read_exact(&mut buf)?;
        newline_count += buf.iter().filter(|&&b| b == b'\n').count();
        chunks.push(buf);
        pos = start;
    }

    chunks.reverse();
    let mut tail = chunks.concat();

    // If we read more than `max_lines` newlines, drop everything up to and
    // including the first newline in the assembled tail. That removes the
    // partial earliest line and leaves exactly the last `max_lines` lines.
    if newline_count > max_lines {
        let mut skip = newline_count - max_lines;
        let mut idx = 0;
        while skip > 0 && idx < tail.len() {
            if tail[idx] == b'\n' {
                skip -= 1;
            }
            idx += 1;
        }
        if idx > 0 {
            tail.drain(..idx);
        }
    }

    let text = String::from_utf8_lossy(&tail).into_owned();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    Ok(lines)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_wraps_tail_read_and_runs_for_missing_blobs() {
        use crate::executor::state::{BlobMode, FileSystemStateStore, StateStore};
        use crate::test_support::Sandbox;
        use std::io::Write;

        let _sandbox = Sandbox::new();
        let store: Arc<dyn StateStore + Send + Sync> = Arc::new(
            FileSystemStateStore::create("gr-blob-snapshot", &serde_json::Map::new(), false)
                .unwrap(),
        );
        store
            .open("log", BlobMode::Write)
            .unwrap()
            .write_all(b"before\n")
            .unwrap();
        let snapshot_store = Arc::clone(&store);
        let (lines, through_sequence) =
            read_store_blob_tail_with_snapshot(&store, "log", 10, move |read| {
                // A write before invoking read must be included in the tail.
                snapshot_store
                    .open("log", BlobMode::Write)
                    .unwrap()
                    .write_all(b"after\n")
                    .unwrap();
                let lines = read();
                snapshot_store
                    .open("log", BlobMode::Write)
                    .unwrap()
                    .write_all(b"later\n")
                    .unwrap();
                (lines, Some(42))
            })
            .await;
        assert_eq!(lines, vec!["after"]);
        assert_eq!(through_sequence, Some(42));
        assert_eq!(
            read_store_blob_tail_with_snapshot(&store, "missing", 10, |read| (read(), Some(43)))
                .await,
            (Vec::new(), Some(43))
        );
        assert_eq!(
            read_store_blob_tail_with_snapshot(&store, "log", 0, |read| (read(), Some(44))).await,
            (Vec::new(), Some(44))
        );
    }

    fn exec_test_store(id: &str) -> Arc<dyn state::StateStore + Send + Sync> {
        Arc::new(state::FileSystemStateStore::create(id, &serde_json::Map::new(), false).unwrap())
    }

    fn write_exec_blob(store: &Arc<dyn state::StateStore + Send + Sync>, name: &str, data: &[u8]) {
        use std::io::Write;
        store
            .open(name, state::BlobMode::Write)
            .unwrap()
            .write_all(data)
            .unwrap();
    }

    fn legacy_output(
        text: &str,
    ) -> gremlins_magic_bus::DispatchEvent<gremlins_magic_bus::EventKind> {
        use gremlins_magic_bus::{CorrId, DispatchEvent, EventKind};
        DispatchEvent::new(
            CorrId::new().with_run("gr-exec-raw"),
            EventKind::ExecOutput {
                id: "gr-exec-raw".into(),
                text: text.into(),
                is_stderr: false,
                ts: "now".into(),
            },
        )
    }

    fn write_legacy_output(
        store: &Arc<dyn state::StateStore + Send + Sync>,
        name: &str,
        text: &str,
    ) -> u64 {
        use gremlins_magic_bus::{Bus, EventKind};
        use std::io::Write;
        let bus = Bus::<EventKind>::new();
        let mut sequence = 0;
        let mut writer = store
            .open(&format!("{name}.events"), state::BlobMode::Write)
            .unwrap();
        bus.emit_with_persistence(legacy_output(text), |event| {
            sequence = event.sequence();
            writeln!(
                writer,
                "{}",
                serde_json::json!({"sequence": event.sequence(), "event": event.kind()})
            )
            .unwrap();
            writer.flush().unwrap();
        });
        sequence
    }

    #[tokio::test]
    async fn exec_tail_preserves_raw_preamble_and_footer() {
        let _sandbox = crate::test_support::Sandbox::new();
        let store = exec_test_store("gr-exec-preamble");
        let name = "command_logs/exec-test.log";
        write_exec_blob(
            &store,
            name,
            b"runner preamble\ncommand output\nrunner footer\n",
        );
        write_legacy_output(&store, name, "command output");

        assert_eq!(
            read_exec_tail(&store, name, 10).await,
            (
                vec![
                    "runner preamble".into(),
                    "command output".into(),
                    "runner footer".into()
                ],
                None,
                None
            )
        );
        assert_eq!(
            read_exec_tail(&store, name, 2).await,
            (
                vec!["command output".into(), "runner footer".into()],
                None,
                None
            )
        );
    }

    #[tokio::test]
    async fn exec_tail_preserves_raw_output_after_sidecar_write_failure() {
        use std::io::Write;
        let _sandbox = crate::test_support::Sandbox::new();
        let store = exec_test_store("gr-exec-sidecar-failure");
        let name = "command_logs/exec-test.log";
        write_exec_blob(&store, name, b"persisted output\n");
        write_legacy_output(&store, name, "persisted output");

        // A read-only sidecar handle deterministically fails the next write.
        let mut failed_writer = store
            .open(&format!("{name}.events"), state::BlobMode::Read)
            .unwrap();
        assert!(failed_writer.write_all(b"next event\n").is_err());
        store
            .open(name, state::BlobMode::Append)
            .unwrap()
            .write_all(b"output after sidecar failure\n")
            .unwrap();

        assert_eq!(
            read_exec_tail(&store, name, 10).await,
            (
                vec![
                    "persisted output".into(),
                    "output after sidecar failure".into()
                ],
                None,
                None
            )
        );
    }

    #[tokio::test]
    async fn exec_tail_ignores_malformed_sidecar_wholesale() {
        use std::io::Write;
        let _sandbox = crate::test_support::Sandbox::new();
        let store = exec_test_store("gr-exec-malformed-sidecar");
        let name = "command_logs/exec-test.log";
        write_exec_blob(&store, name, b"raw output only\n");
        write_legacy_output(&store, name, "valid sidecar prefix must not replay");
        store
            .open(&format!("{name}.events"), state::BlobMode::Append)
            .unwrap()
            .write_all(b"{malformed json\n")
            .unwrap();

        assert_eq!(
            read_exec_tail(&store, name, 10).await,
            (vec!["raw output only".into()], None, None)
        );
    }

    #[tokio::test]
    async fn exec_tail_does_not_cover_colliding_sequences_after_restart() {
        use gremlins_magic_bus::{Bus, EventKind};
        let _sandbox = crate::test_support::Sandbox::new();
        let store = exec_test_store("gr-exec-restart");
        let name = "command_logs/exec-test.log";
        write_exec_blob(&store, name, b"old raw output\n");
        let legacy_sequence = write_legacy_output(&store, name, "old raw output");
        assert_eq!(legacy_sequence, 1);

        let fresh_bus = Bus::<EventKind>::new();
        let mut fresh_sequence = 0;
        fresh_bus.emit_with_persistence(legacy_output("fresh output"), |event| {
            fresh_sequence = event.sequence();
        });
        assert_eq!(fresh_sequence, 1);
        assert_eq!(fresh_sequence, legacy_sequence);

        let (lines, through, covered) = read_exec_tail(&store, name, 10).await;
        assert_eq!(lines, vec!["old raw output"]);
        assert_eq!(through, None);
        assert_eq!(covered, None);
        assert!(!through.is_some_and(|sequence| fresh_sequence <= sequence));
        assert!(!covered
            .as_ref()
            .is_some_and(|sequences| sequences.contains(&fresh_sequence)));
    }

    #[tokio::test]
    async fn exec_tail_empty_reads_never_claim_coverage() {
        let _sandbox = crate::test_support::Sandbox::new();
        let store = exec_test_store("gr-exec-empty");
        let name = "command_logs/exec-test.log";
        write_exec_blob(&store, name, b"raw output\n");
        write_legacy_output(&store, name, "legacy output");
        assert_eq!(
            read_exec_tail(&store, name, 0).await,
            (Vec::new(), None, None)
        );
        assert_eq!(
            read_exec_tail(&store, "missing", 10).await,
            (Vec::new(), None, None)
        );

        let sidecar_only = "command_logs/sidecar-only.log";
        write_legacy_output(&store, sidecar_only, "must not replay without raw log");
        assert_eq!(
            read_exec_tail(&store, sidecar_only, 10).await,
            (Vec::new(), None, None)
        );
        assert_eq!(
            read_exec_tail(&store, sidecar_only, 0).await,
            (Vec::new(), None, None)
        );
    }

    #[test]
    fn read_tail_lines_keeps_only_requested_tail() {
        let mut data = Vec::new();
        for i in 0..10 {
            data.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        let mut cursor = std::io::Cursor::new(data);
        let tail = read_tail_lines(&mut cursor, 3).unwrap();
        assert_eq!(tail, vec!["line 7", "line 8", "line 9"]);
    }

    #[test]
    fn read_tail_lines_handles_no_trailing_newline() {
        let data = b"one\ntwo\nthree";
        let mut cursor = std::io::Cursor::new(data.as_slice());
        let tail = read_tail_lines(&mut cursor, 2).unwrap();
        assert_eq!(tail, vec!["two", "three"]);
    }
}
