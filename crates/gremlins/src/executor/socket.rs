//! Unix-domain socket helpers for the single-instance executor.
//!
//! The socket lives at `$state_root/executor.sock`.
//! All I/O is async via tokio.

use std::os::fd::AsRawFd;
use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Path to the executor socket.
pub fn socket_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join("executor.sock")
}

/// Path to the executor lock file.
pub fn lock_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join("executor.lock")
}

/// Try to acquire the executor's advisory lock without blocking.
///
/// Returns `Ok(Some(file))` when the lock was acquired (it is held for the
/// lifetime of the returned handle), `Ok(None)` when another process holds
/// it, and `Err(_)` on I/O failure.
pub fn try_acquire_lock(state_root: &Path) -> Result<Option<std::fs::File>, String> {
    let path = lock_path(state_root);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("failed to open executor lock {}: {e}", path.display()))?;

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(format!(
            "failed to lock executor lock {}: {err}",
            path.display()
        ));
    }

    Ok(Some(file))
}

/// Bind the executor socket.
///
/// The caller must hold the executor advisory lock. Any stale socket file
/// from a crashed executor is unlinked before binding.
pub fn bind_socket(state_root: &Path) -> Result<UnixListener, String> {
    let path = socket_path(state_root);

    // The caller holds the executor lock, so no live executor exists and any
    // socket file here is stale.
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
