//! Single-instance executor: bind-or-connect and daemon spawn.
//!
//! "bind_or_connect()" is the atomic claim: try "connect()" to the
//! executor socket; if that fails, try to acquire the executor's advisory
//! lock. The process that gets the lock spawns a detached "gremlins serve"
//! daemon (inheriting the lock fd) and then poll-connects like any other
//! client. The daemon holds the lock for its lifetime, so the lock cannot
//! go stale.

use std::os::fd::AsRawFd;

use gremlins::config;
use gremlins::executor::socket;

/// Try to connect to an existing executor. Returns "Ok(stream)" on success,
/// or an error if no executor is running.
pub(crate) async fn connect() -> Result<tokio::net::UnixStream, String> {
    let state_root = config::state_root();
    socket::connect_socket(&state_root).await
}

/// Atomically claim the executor socket: try "connect()", fall back to
/// acquiring the advisory lock. The lock winner spawns a detached
/// "gremlins serve" daemon (passing the lock fd) and then poll-connects.
/// Losers just poll-connect until the daemon is reachable.
pub(crate) async fn bind_or_connect() -> Result<tokio::net::UnixStream, String> {
    // Fast path: an executor is already running.
    match connect().await {
        Ok(stream) => return Ok(stream),
        Err(e) if is_no_socket(&e) || is_connection_refused(&e) => {
            // No executor — try to claim the lock.
        }
        Err(e) => return Err(e),
    }

    let state_root = config::state_root();

    match socket::try_acquire_lock(&state_root) {
        Ok(Some(lock_file)) => {
            // We own the lock — no executor can be running. Spawn a
            // detached daemon that inherits the lock fd and runs the
            // supervisor.
            let lock_fd = lock_file.as_raw_fd();

            // Rust creates fds with FD_CLOEXEC by default. Clear it so
            // the daemon inherits this fd across exec.
            let flags = unsafe { libc::fcntl(lock_fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(format!(
                    "fcntl F_GETFD failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            unsafe {
                libc::fcntl(lock_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }

            let current_exe =
                std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;

            let mut cmd = std::process::Command::new(current_exe);
            cmd.arg("serve").arg(lock_fd.to_string());
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            cmd.spawn()
                .map_err(|e| format!("failed to spawn executor daemon: {e}"))?;

            // The daemon now holds the lock via the same open file
            // description. We must not close our fd reference (that would
            // release the lock). Leak the File handle.
            std::mem::forget(lock_file);

            // Fall through to poll until the daemon is accepting.
            poll_for_executor().await
        }
        Ok(None) => {
            // Another process is becoming (or is) the executor — poll
            // until it is reachable.
            poll_for_executor().await
        }
        Err(e) => Err(e),
    }
}

/// Poll "connect()" with a short sleep between attempts until the executor
/// is reachable or we give up.
async fn poll_for_executor() -> Result<tokio::net::UnixStream, String> {
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
