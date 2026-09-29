//! Single-instance executor: bind-or-connect and daemon spawn.
//!
//! `bind_or_connect()` is the atomic claim: try `connect()` to the
//! executor socket; if that fails, try to acquire the executor's advisory
//! lock. The process that gets the lock spawns a detached `gremlins serve`
//! daemon (inheriting the lock fd) and then poll-connects like any other
//! client. Lock losers retry the full connect+acquire decision every poll
//! iteration so a waiter can take over if the current holder exits before
//! publishing the socket.

use std::os::fd::{AsRawFd, FromRawFd};

use gremlins::config;
use gremlins::executor::socket;

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
        match socket::try_acquire_lock(&state_root) {
            Ok(Some(lock_file)) => {
                spawn_daemon(lock_file)?;
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

/// Spawn the executor daemon as a detached child process.
///
/// The daemon inherits the lock fd. This function dups the lock fd above
/// the standard descriptor range (>= 3) so that `Stdio::null()` setup in
/// the child does not replace the lock fd. It also starts the daemon in a
/// separate process group so that terminal signals (Ctrl-C) sent to the
/// CLI do not kill the executor and all hosted gremlins.
fn spawn_daemon(lock_file: std::fs::File) -> Result<(), String> {
    let lock_fd = lock_file.as_raw_fd();

    // Dup the lock fd above the standard descriptor range so that the
    // child's Stdio::null() setup (which may replace fds 0, 1, 2) does
    // not clobber the lock file descriptor.
    let safe_fd = unsafe { libc::fcntl(lock_fd, libc::F_DUPFD_CLOEXEC, 3) };
    if safe_fd < 0 {
        return Err(format!(
            "failed to dup lock fd: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Close the original fd; safe_fd refers to the same open file
    // description so the lock is still held.
    drop(lock_file);

    // Clear FD_CLOEXEC so the daemon inherits this fd across exec.
    let flags = unsafe { libc::fcntl(safe_fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "fcntl F_GETFD failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    unsafe {
        libc::fcntl(safe_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }

    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;

    let mut cmd = std::process::Command::new(current_exe);
    cmd.arg("serve").arg(safe_fd.to_string());
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    // Start the executor in a separate process group so that terminal
    // signals (Ctrl-C, group shutdown) sent to the CLI do not kill the
    // daemon and all hosted gremlins.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn executor daemon: {e}"))?;

    // The daemon now holds the lock via the dup'd fd referring to the
    // same open file description. Once the child is spawned it has its
    // own reference, so we can safely close safe_fd — but leaking is
    // harmless and avoids a close race. Wrap in a File and forget.
    let _leak = unsafe { std::fs::File::from_raw_fd(safe_fd) };
    std::mem::forget(_leak);

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