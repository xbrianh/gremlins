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

/// Become the executor: bind the socket, write the pidfile, and start serving.
///
/// This function blocks until the executor shuts down (run map goes empty).
pub(crate) async fn serve() -> Result<(), String> {
    let state_root = config::state_root();

    // Write pidfile.
    socket::write_pidfile(&state_root)?;

    // Bind the socket.
    let listener = socket::bind_socket(&state_root)?;

    log::info!(
        "executor: listening on {}",
        socket::socket_path(&state_root).display()
    );

    // Run the supervisor accept loop.
    supervisor::run_supervisor(listener, state_root).await;

    Ok(())
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
