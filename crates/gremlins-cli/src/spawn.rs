//! Single-instance executor: bind-or-connect and serve.
//!
//! `bind_or_connect()` implements the atomic claim from `single-instance-forward.md`:
//! try `connect()` to the executor socket; if that fails, `bind()` it and
//! become the executor. `EADDRINUSE` means another process won the race —
//! retry connect.
//!
//! `serve()` runs the accept loop, calling into the supervisor.

use gremlins::config;
use gremlins::executor::socket;
use gremlins::executor::supervisor;

/// Try to connect to an existing executor. Returns `Ok(stream)` on success,
/// or an error if no executor is running.
pub(crate) async fn connect() -> Result<tokio::net::UnixStream, String> {
    let state_root = config::state_root();
    socket::connect_socket(&state_root).await
}

/// Atomically claim the executor socket: try `connect()`, fall back to
/// `bind()`, and retry `connect()` on `EADDRINUSE`. Returns a connected
/// stream to the executor (either an existing one or the one we just
/// started).
pub(crate) async fn bind_or_connect() -> Result<tokio::net::UnixStream, String> {
    // Fast path: an executor is already running.
    match connect().await {
        Ok(stream) => return Ok(stream),
        Err(e) if is_no_socket(&e) || is_connection_refused(&e) => {
            // No executor — try to become one.
        }
        Err(e) => return Err(e),
    }

    let state_root = config::state_root();

    // Write pidfile and try to bind.
    socket::write_pidfile(&state_root)?;
    let listener = match socket::bind_socket(&state_root) {
        Ok(l) => l,
        Err(e) if is_addr_in_use(&e) => {
            // Another process won the race — retry connect.
            return connect().await;
        }
        Err(e) => return Err(e),
    };

    log::info!(
        "executor: listening on {}",
        socket::socket_path(&state_root).display()
    );

    // We won the race — spawn the supervisor in the background.
    tokio::spawn(async move {
        supervisor::run_supervisor(listener, state_root).await;
    });

    // Poll until our own supervisor is accepting connections.
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        match connect().await {
            Ok(stream) => return Ok(stream),
            Err(_) => continue,
        }
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

/// Check if the error is "address in use" (another process bound first).
pub(crate) fn is_addr_in_use(error: &str) -> bool {
    error.contains("Address already in use")
        || error.contains("address already in use")
        || error.contains("EADDRINUSE")
}
