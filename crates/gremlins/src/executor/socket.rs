//! Unix-domain socket helpers for the single-instance executor.
//!
//! The socket lives at `$state_root/executor.sock`.
//! All I/O is async via tokio.

use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Path to the executor socket.
pub fn socket_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join("executor.sock")
}

/// Bind the executor socket.
///
/// The caller must have already confirmed no executor is listening
/// (i.e. `connect_socket` returned ECONNREFUSED or ENOENT). Any
/// stale socket file is unlinked before binding.
pub fn bind_socket(state_root: &Path) -> Result<UnixListener, String> {
    let path = socket_path(state_root);

    // If a socket file is left over from a crashed executor, remove it.
    // We know it's stale because the caller already tried connect().
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    UnixListener::bind(&path)
        .map_err(|e| format!("failed to bind executor socket {}: {e}", path.display()))
}

/// Connect to the executor socket.
pub async fn connect_socket(state_root: &Path) -> Result<UnixStream, String> {
    let path = socket_path(state_root);
    UnixStream::connect(&path)
        .await
        .map_err(|e| format!("failed to connect to executor at {}: {e}", path.display()))
}

/// Read one JSON-line from an async buffered reader.
///
/// Returns `None` on EOF (clean shutdown). Empty lines are skipped.
pub async fn read_json_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed)
            .map(Some)
            .map_err(|e| format!("invalid JSON: {e}"));
    }
}

/// Write one JSON-line to an async writer.
pub async fn write_json_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| format!("JSON encode: {e}"))?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("write error: {e}"))
}

/// Remove the socket file, best-effort.
pub fn unlink_socket(state_root: &Path) {
    let _ = std::fs::remove_file(socket_path(state_root));
}
