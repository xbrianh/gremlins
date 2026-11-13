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
pub(crate) async fn read_store_blob_tail(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    name: &str,
    max_lines: usize,
) -> Vec<String> {
    if max_lines == 0 || !store.exists(name) {
        return Vec::new();
    }

    let store = Arc::clone(store);
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let mut blob = match store.open(&name, state::BlobMode::Read) {
            Ok(blob) => blob,
            Err(_) => return Vec::new(),
        };
        read_tail_lines(&mut *blob, max_lines).unwrap_or_default()
    })
    .await
    .unwrap_or_default()
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
