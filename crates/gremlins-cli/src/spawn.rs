//! Single-instance executor: bind-or-connect and daemon spawn.
//!
//! `bind_or_connect()` is the atomic claim: try `connect()` to the
//! executor socket; if that fails, try to acquire the executor's advisory
//! lock. The process that gets the lock spawns a detached `gremlins serve`
//! daemon (inheriting the lock fd) and then poll-connects like any other
//! client. Lock losers retry the full connect+acquire decision every poll
//! iteration so a waiter can take over if the current holder exits before
//! publishing the socket.

use gremlins::config;
use gremlins::executor::socket::{self, GremlinsDaemonLock};

/// Try to connect to an existing executor. Returns `Ok(stream)` on success,
/// or an error if no executor is running.
pub(crate) async fn connect() -> Result<tokio::net::UnixStream, String> {
    let state_root = config::state_root();
    socket::connect_socket(&state_root).await
}

/// Atomically claim the executor socket: try `connect()`, fall back to
/// acquiring the advisory lock. The lock winner spawns a detached
/// `gremlins serve` daemon (passing the lock fd) and then poll-connects.
/// Lock losers retry the full connect+acquire decision every iteration
/// so a waiter can take over if the holder exits before binding.
pub(crate) async fn bind_or_connect() -> Result<tokio::net::UnixStream, String> {
    let state_root = config::state_root();

    for _ in 0..100 {
        // Fast path: an executor is already running.
        match connect().await {
            Ok(stream) => return Ok(stream),
            Err(e) if is_no_socket(&e) || is_connection_refused(&e) => {
                // No executor — try to claim the lock below.
            }
            Err(e) => return Err(e),
        }

        // Try to acquire the lock.
        match GremlinsDaemonLock::try_acquire(&state_root) {
            Ok(Some(lock)) => {
                lock.spawn_daemon()?;
                // Daemon spawned — continue polling until it is
                // accepting. If the daemon exits before binding, the
                // next iteration will see ECONNREFUSED, re-acquire the
                // now-free lock, and spawn a replacement.
            }
            Ok(None) => {
                // Another process is (or is becoming) the executor.
            }
            Err(e) => return Err(e),
        }

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    Err("executor failed to start".to_string())
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
