//! Unix-domain socket helpers for the single-instance executor.
//!
//! The socket lives at `$state_root/executor.sock`.
//! All I/O is async via tokio.

use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Path to the executor socket.
pub fn socket_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join("executor.sock")
}

/// Path to the executor lock file.
///
/// Prefer [`GremlinsDaemonLock::lock_path`] for new code; this free
/// function remains for callers that only need the path.
#[doc(hidden)]
pub fn lock_path(state_root: &Path) -> std::path::PathBuf {
    GremlinsDaemonLock::lock_path(state_root)
}

/// Owns the executor's exclusive advisory flock on `executor.lock`.
///
/// The lock serializes executor startup: the process that acquires it
/// becomes the executor daemon. The lock is held for the daemon's
/// lifetime and released by the kernel when the daemon exits.
///
/// # Handoff protocol
///
/// The parent CLI acquires the lock, dups the fd to a safe range, and
/// spawns `gremlins serve <fd>`. The child validates the inherited fd
/// and reconstructs the lock. Both sides use this type:
///
///   - Parent: [`try_acquire`](Self::try_acquire) → [`spawn_daemon`](Self::spawn_daemon)
///   - Child:  [`from_inherited_fd`](Self::from_inherited_fd)
pub struct GremlinsDaemonLock {
    file: std::fs::File,
}

impl GremlinsDaemonLock {
    /// Path to the executor lock file.
    pub fn lock_path(state_root: &Path) -> std::path::PathBuf {
        state_root.join("executor.lock")
    }

    /// Try to acquire the executor's exclusive advisory lock without
    /// blocking.
    ///
    /// Returns `Ok(Some(lock))` when the lock was acquired, `Ok(None)`
    /// when another process holds it, and `Err(_)` on I/O failure.
    pub fn try_acquire(state_root: &Path) -> Result<Option<Self>, String> {
        let path = Self::lock_path(state_root);
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

        Ok(Some(Self { file }))
    }

    /// Validate an inherited fd and construct a lock from it.
    ///
    /// The fd must be open, refer to the canonical lock file on disk,
    /// and hold an exclusive lock. Used by the daemon child
    /// (`gremlins serve <fd>`) to validate the fd received from the
    /// parent.
    pub fn from_inherited_fd(fd: i32, state_root: &Path) -> Result<Self, String> {
        // 1. Check the fd is open.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!(
                "invalid lock fd {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }

        // 2. Check the fd refers to the canonical lock file.
        let lock_path = Self::lock_path(state_root);
        let mut fd_stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut fd_stat) } < 0 {
            return Err(format!(
                "fstat on lock fd {fd} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let lock_path_c = std::ffi::CString::new(lock_path.as_os_str().as_encoded_bytes())
            .map_err(|_| "lock path contains nul byte".to_string())?;
        let mut path_stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(lock_path_c.as_ptr(), &mut path_stat) } == 0
            && (fd_stat.st_ino != path_stat.st_ino || fd_stat.st_dev != path_stat.st_dev)
        {
            return Err("lock fd does not refer to the executor lock file".to_string());
        }

        // 3. Check the fd holds an exclusive lock.
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .map_err(|e| format!("cannot open lock file for validation: {e}"))?;
        let ret = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
            return Err("lock fd does not hold the executor lock".to_string());
        }

        // Safety: we've validated the fd is open, refers to the right
        // file, and holds the lock.
        Ok(Self {
            file: unsafe { std::fs::File::from_raw_fd(fd) },
        })
    }

    /// Dup the lock fd to a safe range (≥ 3) and clear `FD_CLOEXEC` so
    /// it can be passed to a child process via fork+exec.
    ///
    /// Returns the dup'd fd number. The caller must either close or
    /// leak the returned fd after the child has been spawned.
    fn handoff_fd(&self) -> Result<i32, String> {
        let raw = self.file.as_raw_fd();

        // Dup above the standard descriptor range so that the child's
        // Stdio::null() setup (which may replace fds 0, 1, 2) does not
        // clobber the lock file descriptor.
        let safe_fd = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
        if safe_fd < 0 {
            return Err(format!(
                "failed to dup lock fd: {}",
                std::io::Error::last_os_error()
            ));
        }

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

        Ok(safe_fd)
    }

    /// Spawn the executor daemon as a detached child process, passing
    /// the lock fd so the daemon can continue holding the exclusive
    /// lock for its lifetime.
    ///
    /// The daemon is started in a separate process group so that
    /// terminal signals (Ctrl‑C) sent to the CLI do not kill the
    /// executor and all hosted gremlins.
    pub fn spawn_daemon(&self) -> Result<(), String> {
        let safe_fd = self.handoff_fd()?;

        let current_exe =
            std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;

        let mut cmd = std::process::Command::new(current_exe);
        cmd.arg("serve").arg(safe_fd.to_string());
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        cmd.spawn()
            .map_err(|e| format!("failed to spawn executor daemon: {e}"))?;

        // The daemon now holds the lock via the dup'd fd referring to
        // the same open file description. Once the child is spawned it
        // has its own reference, so we can safely close safe_fd — but
        // leaking is harmless and avoids a close race.
        let _leak = unsafe { std::fs::File::from_raw_fd(safe_fd) };
        std::mem::forget(_leak);

        Ok(())
    }
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
