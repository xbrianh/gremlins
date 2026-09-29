//! Unix-domain socket helpers for the single-instance executor.
//!
//! The socket lives at `$state_root/executor.sock`. A pidfile at
//! `$state_root/executor.pid` records the executor's PID so a crashed
//! executor's stale socket can be detected and reclaimed.
//!
//! All I/O is async via tokio.

use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Path to the executor socket.
pub fn socket_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join("executor.sock")
}

/// Path to the executor pidfile.
pub fn pidfile_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join("executor.pid")
}

/// Write the current PID to the pidfile.
pub fn write_pidfile(state_root: &Path) -> Result<(), String> {
    let path = pidfile_path(state_root);
    let pid = std::process::id();
    std::fs::write(&path, pid.to_string())
        .map_err(|e| format!("failed to write pidfile {}: {e}", path.display()))
}

/// Remove the pidfile.
pub fn unlink_pidfile(state_root: &Path) {
    let path = pidfile_path(state_root);
    let _ = std::fs::remove_file(&path);
}

/// Check whether the PID recorded in the pidfile is still alive.
///
/// Returns `true` when the pidfile exists and the process it names is still
/// running. Returns `false` when the pidfile is missing, unreadable, or the
/// process is gone — in all of those cases the socket can be reclaimed.
pub fn pidfile_alive(state_root: &Path) -> bool {
    let path = pidfile_path(state_root);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let pid: libc::pid_t = match content.trim().parse() {
        Ok(p) if p > 0 => p,
        _ => return false,
    };
    // kill(pid, 0) checks existence without sending a signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Bind the executor socket, reclaiming a stale socket if the previous
/// executor is dead.
pub fn bind_socket(state_root: &Path) -> Result<UnixListener, String> {
    let path = socket_path(state_root);

    // If the socket file exists but the executor is dead, unlink it.
    if path.exists() && !pidfile_alive(state_root) {
        let _ = std::fs::remove_file(&path);
        unlink_pidfile(state_root);
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
