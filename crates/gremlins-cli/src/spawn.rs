//! Single-instance executor: bind-or-connect and daemon spawn.
//!
//! `bind_or_connect()` is the atomic claim: try `connect()` to the
//! executor socket; if that fails, try to acquire the executor's advisory
//! lock. The process that gets the lock spawns a `gremlins serve` daemon
//! (inheriting the lock fd) and waits for the daemon to print `ready`
//! to stdout — a deterministic readiness signal. Lock losers poll-connect
//! until the winner publishes the socket.

use std::os::fd::{FromRawFd, IntoRawFd};
use std::path::Path;

use gremlins::config;
use gremlins::executor::socket::{self, GremlinsDaemonLock};

/// Try to connect to an existing executor. Returns `Ok(stream)` on success,
/// or an error if no executor is running.
pub(crate) async fn connect() -> Result<tokio::net::UnixStream, String> {
    let state_root = config::state_root();
    socket::connect_socket(&state_root).await
}

/// Atomically claim the executor socket: try `connect()`, fall back to
/// acquiring the advisory lock. The lock winner spawns a `gremlins serve`
/// daemon (passing the lock fd) and waits for the daemon to print `ready`
/// to stdout — a deterministic readiness signal. Lock losers poll-connect
/// until the winner publishes the socket.
pub(crate) async fn bind_or_connect() -> Result<tokio::net::UnixStream, String> {
    let state_root = config::state_root();

    // Fast path: an executor is already running.
    if let Ok(stream) = connect().await {
        return Ok(stream);
    }

    // No executor — try to claim the lock and become the executor.
    loop {
        match GremlinsDaemonLock::try_acquire(&state_root) {
            Ok(Some(lock)) => {
                return spawn_and_wait(lock, &state_root).await;
            }
            Ok(None) => {
                // Another process holds the lock — it may be starting
                // the daemon. Poll-connect until it's ready.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                match connect().await {
                    Ok(stream) => return Ok(stream),
                    Err(e) if is_no_socket(&e) || is_connection_refused(&e) => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Spawn the daemon and wait for its readiness signal on stdout.
///
/// The daemon prints `ready` to stdout after binding the socket. If the
/// daemon exits before printing it, we capture stderr and return the
/// diagnostics. A 10-second timeout guards against a hung daemon.
async fn spawn_and_wait(
    lock: GremlinsDaemonLock,
    state_root: &Path,
) -> Result<tokio::net::UnixStream, String> {
    use tokio::io::AsyncBufReadExt;

    let mut child = lock.spawn_daemon()?;

    let stdout = child
        .stdout
        .take()
        .ok_or("failed to capture daemon stdout")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("failed to capture daemon stderr")?;

    let mut async_stdout = tokio::io::BufReader::new(tokio::fs::File::from_std(unsafe {
        std::fs::File::from_raw_fd(stdout.into_raw_fd())
    }));

    // Read the readiness line with a 10-second timeout.
    let mut line = String::new();
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async_stdout.read_line(&mut line),
    )
    .await;

    match read_result {
        Ok(Ok(0)) => {
            // EOF: daemon exited before printing "ready".
            let err_text = drain_stderr(stderr).await;
            Err(if err_text.is_empty() {
                "executor daemon exited before binding".to_string()
            } else {
                format!("executor daemon exited before binding: {err_text}")
            })
        }
        Ok(Ok(_)) => {
            let trimmed = line.trim();
            if trimmed == "ready" {
                // Drop the lock so only the daemon holds it.
                drop(lock);
                return socket::connect_socket(state_root).await;
            }
            // Kill the daemon before draining stderr — if it's still
            // running, read_to_string would block forever.
            let _ = child.kill();
            let _ = child.wait();
            let err_text = drain_stderr(stderr).await;
            let detail = if err_text.is_empty() {
                String::new()
            } else {
                format!("; stderr: {err_text}")
            };
            Err(format!(
                "unexpected daemon stdout: {trimmed:?} (expected \"ready\"){detail}"
            ))
        }
        Ok(Err(e)) => {
            // Kill the daemon before draining stderr — if it's still
            // running, read_to_string would block forever.
            let _ = child.kill();
            let _ = child.wait();
            let err_text = drain_stderr(stderr).await;
            Err(if err_text.is_empty() {
                format!("failed to read daemon stdout: {e}")
            } else {
                format!("failed to read daemon stdout: {e}; stderr: {err_text}")
            })
        }
        Err(_elapsed) => {
            // Kill the daemon before draining stderr — the daemon is
            // still running (it just didn't print "ready" within the
            // timeout window), so read_to_string would block forever.
            let _ = child.kill();
            let _ = child.wait();
            let err_text = drain_stderr(stderr).await;
            Err(if err_text.is_empty() {
                "executor daemon timed out waiting for readiness".to_string()
            } else {
                format!("executor daemon timed out; stderr: {err_text}")
            })
        }
    }
}

/// Drain the daemon's stderr pipe into a trimmed string.
async fn drain_stderr(stderr: std::process::ChildStderr) -> String {
    use tokio::io::AsyncReadExt;
    let mut async_stderr =
        tokio::fs::File::from_std(unsafe { std::fs::File::from_raw_fd(stderr.into_raw_fd()) });
    let mut buf = String::new();
    let _ = async_stderr.read_to_string(&mut buf).await;
    buf.trim().to_string()
}

/// Send a JSON-line request to the executor and read one reply.
pub(crate) async fn send_request(
    stream: &mut tokio::net::UnixStream,
    request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    socket::write_json_line(stream, &request).await?;
    let mut reader = tokio::io::BufReader::new(&mut *stream);
    match socket::read_json_line(&mut reader).await? {
        Some(v) => Ok(v),
        None => Err("executor closed connection".to_string()),
    }
}

/// Check if the error is "connection refused" (no executor running).
pub(crate) fn is_connection_refused(error: &str) -> bool {
    error.contains("Connection refused")
        || error.contains("connection refused")
        || error.contains("ECONNREFUSED")
}

/// Check if the error is "no such file" (socket doesn't exist).
pub(crate) fn is_no_socket(error: &str) -> bool {
    error.contains("No such file") || error.contains("ENOENT") || error.contains("not found")
}
