use std::collections::HashMap;
use std::io;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Return type for `run`.
#[derive(Debug)]
pub struct ProcResult {
    pub returncode: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Error type for `run`.
#[derive(Debug)]
pub enum ProcError {
    CalledProcessError(i32, Vec<u8>, Vec<u8>),
    TimeoutExpired(f64, Vec<u8>, Vec<u8>),
    Io(io::Error),
    EmptyCommand,
    InvalidTimeout(f64),
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcError::CalledProcessError(rc, _, _) => {
                write!(f, "Command returned non-zero exit status {rc}")
            }
            ProcError::TimeoutExpired(timeout, _, _) => {
                write!(f, "Command timed out after {timeout}s")
            }
            ProcError::Io(e) => e.fmt(f),
            ProcError::EmptyCommand => write!(f, "empty command"),
            ProcError::InvalidTimeout(t) => {
                write!(f, "timeout must be a finite non-negative number, got {t}")
            }
        }
    }
}

impl std::error::Error for ProcError {}

/// The Python-style returncode for a finished process: its exit code, or the
/// negated signal number when a signal killed it. Mirrors
/// `asyncio.subprocess.Process.returncode`, which the parallel supervisor
/// relies on to name a signal-terminated child.
pub fn exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

pub fn run_or_raise(cmd: &[String], cwd: Option<&Path>) -> Result<String, ProcError> {
    let r = run(cmd, cwd, true, None)?;
    Ok(String::from_utf8_lossy(&r.stdout).trim().to_string())
}

pub fn run_quiet(cmd: &[String], cwd: Option<&Path>) -> Result<ProcResult, ProcError> {
    if cmd.is_empty() {
        return Err(ProcError::EmptyCommand);
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    let status = c.status().map_err(ProcError::Io)?;
    Ok(ProcResult {
        returncode: exit_code(&status),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

pub fn run_ok(cmd: &[String], cwd: Option<&Path>) -> Result<bool, io::Error> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    let status = c.status()?;
    Ok(status.success())
}

pub fn run(
    cmd: &[String],
    cwd: Option<&Path>,
    check: bool,
    timeout: Option<f64>,
) -> Result<ProcResult, ProcError> {
    if cmd.is_empty() {
        return Err(ProcError::EmptyCommand);
    }
    if let Some(t) = timeout {
        if !t.is_finite() || t < 0.0 {
            return Err(ProcError::InvalidTimeout(t));
        }
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.stdout(std::process::Stdio::piped());
    c.stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c.spawn().map_err(ProcError::Io)?;

    let output = match timeout {
        Some(t) => run_with_timeout(&mut child, t)?,
        None => {
            let output = child.wait_with_output().map_err(ProcError::Io)?;
            ProcResult {
                returncode: exit_code(&output.status),
                stdout: output.stdout,
                stderr: output.stderr,
            }
        }
    };

    if check && output.returncode != 0 {
        return Err(ProcError::CalledProcessError(
            output.returncode,
            output.stdout,
            output.stderr,
        ));
    }
    Ok(output)
}

fn run_with_timeout(
    child: &mut std::process::Child,
    timeout_s: f64,
) -> Result<ProcResult, ProcError> {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);

    // Take pipes and read them in concurrent threads so the child won't
    // deadlock by filling the pipe buffer while we wait.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_handle = stdout.map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });

    let stderr_handle = stderr.map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });

    let status = loop {
        match child.try_wait().map_err(ProcError::Io)? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    match status {
        Some(status) => {
            // Child exited normally; collect buffered output.
            let stdout_buf = stdout_handle
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            let stderr_buf = stderr_handle
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            Ok(ProcResult {
                returncode: exit_code(&status),
                stdout: stdout_buf,
                stderr: stderr_buf,
            })
        }
        None => {
            // Kill the whole process group so descendants can't keep pipes open.
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as i32, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();

            let stdout_buf = stdout_handle
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            let stderr_buf = stderr_handle
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            Err(ProcError::TimeoutExpired(timeout_s, stdout_buf, stderr_buf))
        }
    }
}

struct CancelToken {
    pid: u32,
    disarmed: bool,
}

impl CancelToken {
    fn new(pid: u32) -> Self {
        CancelToken {
            pid,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        if !self.disarmed {
            #[cfg(unix)]
            unsafe {
                libc::killpg(self.pid as i32, libc::SIGKILL);
            }
        }
    }
}

pub async fn run_ok_async(cmd: &[String], cwd: Option<&Path>) -> Result<bool, io::Error> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let mut c = tokio::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    c.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.as_std_mut().process_group(0);
    }
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    let status = c.status().await?;
    Ok(status.success())
}

pub async fn run_async(
    cmd: &[String],
    cwd: Option<&Path>,
    check: bool,
    timeout: Option<f64>,
    _text: bool,
    env: Option<&HashMap<String, String>>,
) -> Result<ProcResult, ProcError> {
    if cmd.is_empty() {
        return Err(ProcError::EmptyCommand);
    }
    if let Some(t) = timeout {
        if !t.is_finite() || t < 0.0 || t > Duration::MAX.as_secs_f64() {
            return Err(ProcError::InvalidTimeout(t));
        }
    }

    let mut command = tokio::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    if let Some(e) = env {
        command.env_clear();
        command.envs(e.iter());
    }

    let mut child = command.spawn().map_err(ProcError::Io)?;
    let pid = child
        .id()
        .ok_or_else(|| ProcError::Io(io::Error::other("child process has no pid")))?;
    let mut cancel = CancelToken::new(pid);

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let stdout_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });

    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    let wait_result = match timeout {
        Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t), child.wait()).await {
            Ok(result) => result.map_err(ProcError::Io),
            Err(_elapsed) => {
                #[cfg(unix)]
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
                let _ = child.kill().await;
                let _ = child.wait().await;

                let drain = async {
                    let stdout_buf = stdout_handle.await.unwrap_or_default();
                    let stderr_buf = stderr_handle.await.unwrap_or_default();
                    (stdout_buf, stderr_buf)
                };
                let (stdout_buf, stderr_buf): (Vec<u8>, Vec<u8>) =
                    tokio::time::timeout(Duration::from_secs(5), drain)
                        .await
                        .unwrap_or_default();
                cancel.disarm();
                return Err(ProcError::TimeoutExpired(t, stdout_buf, stderr_buf));
            }
        },
        None => child.wait().await.map_err(ProcError::Io),
    };

    let status = wait_result?;
    cancel.disarm();

    let drain = async {
        let stdout_buf = stdout_handle.await.unwrap_or_default();
        let stderr_buf = stderr_handle.await.unwrap_or_default();
        (stdout_buf, stderr_buf)
    };
    let (stdout_buf, stderr_buf): (Vec<u8>, Vec<u8>) =
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap_or_default();

    let rc = exit_code(&status);
    let result = ProcResult {
        returncode: rc,
        stdout: stdout_buf,
        stderr: stderr_buf,
    };

    if check && rc != 0 {
        Err(ProcError::CalledProcessError(
            rc,
            result.stdout,
            result.stderr,
        ))
    } else {
        Ok(result)
    }
}

pub async fn run_shell_async(
    shell_cmd: &str,
    cwd: Option<&Path>,
    env: Option<&HashMap<String, String>>,
    timeout: Option<f64>,
) -> Result<ProcResult, ProcError> {
    if shell_cmd.is_empty() {
        return Err(ProcError::EmptyCommand);
    }
    if let Some(t) = timeout {
        if !t.is_finite() || t < 0.0 || t > Duration::MAX.as_secs_f64() {
            return Err(ProcError::InvalidTimeout(t));
        }
    }

    let mut command = tokio::process::Command::new("sh");
    command.arg("-c");
    command.arg(shell_cmd);
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    if let Some(e) = env {
        command.env_clear();
        command.envs(e.iter());
    }

    let t0 = Instant::now();
    log::info!(
        "run_shell_async: starting pid=soon cwd={cwd:?} timeout={timeout:?}s cmd={:.200}",
        shell_cmd
    );

    let mut child = command.spawn().map_err(ProcError::Io)?;
    let pid = child
        .id()
        .ok_or_else(|| ProcError::Io(io::Error::other("child process has no pid")))?;
    let mut cancel = CancelToken::new(pid);

    log::info!("run_shell_async: pid={pid} spawned");

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let stdout_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });

    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });

    let heartbeat_handle = {
        let h = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                log::info!(
                    "run_shell_async: pid={pid} heartbeat elapsed={:.0}s",
                    t0.elapsed().as_secs_f64()
                );
            }
        });
        h
    };

    let timeout_warning_handle = timeout.map(|t| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(t * 0.8)).await;
            log::warn!(
                "run_shell_async: pid={pid} approaching timeout ({:.1}s elapsed of {:.1}s)",
                t0.elapsed().as_secs_f64(),
                t
            );
        })
    });

    let wait_result = match timeout {
        Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t), child.wait()).await {
            Ok(result) => result.map_err(ProcError::Io),
            Err(_elapsed) => {
                let elapsed = t0.elapsed().as_secs_f64();
                #[cfg(unix)]
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
                let _ = child.kill().await;
                let _ = child.wait().await;

                heartbeat_handle.abort();
                if let Some(h) = timeout_warning_handle {
                    h.abort();
                }

                let drain = async {
                    let stdout_buf = stdout_handle.await.unwrap_or_default();
                    let stderr_buf = stderr_handle.await.unwrap_or_default();
                    (stdout_buf, stderr_buf)
                };
                let (stdout_buf, stderr_buf): (Vec<u8>, Vec<u8>) =
                    tokio::time::timeout(Duration::from_secs(5), drain)
                        .await
                        .unwrap_or_default();

                log::warn!(
                    "run_shell_async: pid={pid} timed out after {elapsed:.2}s (timeout={t}) stdout_so_far={} stderr_so_far={}",
                    stdout_buf.len(),
                    stderr_buf.len(),
                );

                cancel.disarm();
                return Err(ProcError::TimeoutExpired(t, stdout_buf, stderr_buf));
            }
        },
        None => child.wait().await.map_err(ProcError::Io),
    };

    heartbeat_handle.abort();
    if let Some(h) = timeout_warning_handle {
        h.abort();
    }

    let status = wait_result?;
    cancel.disarm();

    let drain = async {
        let stdout_buf = stdout_handle.await.unwrap_or_default();
        let stderr_buf = stderr_handle.await.unwrap_or_default();
        (stdout_buf, stderr_buf)
    };
    let (stdout_buf, stderr_buf): (Vec<u8>, Vec<u8>) =
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap_or_default();
    let rc = exit_code(&status);
    let elapsed = t0.elapsed().as_secs_f64();

    log::info!(
        "run_shell_async: pid={pid} done in {elapsed:.2}s rc={rc} stdout={} stderr={}",
        stdout_buf.len(),
        stderr_buf.len(),
    );

    Ok(ProcResult {
        returncode: rc,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Poll whether a process with the given PID is still alive, without reaping it.
/// Uses `kill(pid, 0)` which returns 0 if the process exists.
/// Returns `true` if the process is still alive after `timeout`, `false` if it has exited.
#[cfg(unix)]
async fn poll_process_alive(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: kill(pid, 0) is safe; only checks existence, sends no signal.
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive {
            return false;
        }
        if Instant::now() >= deadline {
            return true; // still alive after timeout
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Send SIGTERM → wait grace_s → SIGKILL. Cancellation-safe: the kill sequence
/// runs in a detached tokio task so it completes even if the caller drops the future.
#[cfg(unix)]
pub async fn terminate_with_grace(pid: u32, grace_s: f64) {
    if pid > i32::MAX as u32 || !grace_s.is_finite() || grace_s < 0.0 {
        return;
    }
    let pid_i32 = pid as i32;
    let grace = Duration::from_secs_f64(grace_s);

    let handle = tokio::spawn(async move {
        // Send SIGTERM to the specific PID only (not the process group).
        let ret = unsafe { libc::kill(pid_i32, libc::SIGTERM) };
        if ret != 0 {
            // ESRCH (process already dead) or EPERM — nothing to do.
            return;
        }

        // Wait for the grace period without reaping the child — the caller
        // (e.g. a `Child` handle) will reap it via `waitpid`.
        if poll_process_alive(pid_i32, grace).await {
            // Grace period expired; escalate to SIGKILL.
            unsafe {
                libc::kill(pid_i32, libc::SIGKILL);
            }
            // Give the kernel a moment to deliver the signal (again, no reap).
            let _ = poll_process_alive(pid_i32, Duration::from_millis(500)).await;
        }
    });

    // Await the spawned task — if the caller is cancelled, the spawned task
    // continues running independently.
    let _ = handle.await;
}

/// No-op stub for non-Unix platforms.
#[cfg(not(unix))]
pub async fn terminate_with_grace(_pid: u32, _grace_s: f64) {}

/// Blocking variant of [`terminate_with_grace`] for synchronous contexts (such
/// as a `Drop` impl) where no async runtime is available.
///
/// It sends SIGTERM to the specific PID, waits `grace_s` for the process to
/// exit, then escalates to SIGKILL. It never touches an event loop, so it is
/// safe to call from a thread that does not own the process's asyncio loop.
#[cfg(unix)]
pub fn terminate_with_grace_blocking(pid: u32, grace_s: f64) {
    if pid > i32::MAX as u32 || !grace_s.is_finite() || grace_s < 0.0 {
        return;
    }
    let pid_i32 = pid as i32;
    // SAFETY: kill(pid, SIGTERM) is safe; it targets a single PID.
    if unsafe { libc::kill(pid_i32, libc::SIGTERM) } != 0 {
        // ESRCH (already dead) or EPERM — nothing to do.
        return;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(grace_s);
    loop {
        // SAFETY: kill(pid, 0) only probes for existence.
        if unsafe { libc::kill(pid_i32, 0) } != 0 {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Grace period expired; escalate to SIGKILL.
    unsafe {
        libc::kill(pid_i32, libc::SIGKILL);
    }
    // Give the kernel a moment to deliver the signal.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid_i32, 0) } != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// No-op stub for non-Unix platforms.
#[cfg(not(unix))]
pub fn terminate_with_grace_blocking(_pid: u32, _grace_s: f64) {}

/// Size of each pipe read in [`pump_prefixed`].
const PUMP_CHUNK: usize = 4096;

/// How much unterminated text [`pump_prefixed`] buffers before flushing it
/// anyway, so a child that reports progress without newlines stays live rather
/// than growing our buffer (and, at the extreme, stalling on a full pipe).
pub const MAX_PENDING_BYTES: usize = 64 * 1024;

/// Spawn `python_exe -m gremlins.spawn.child <spec_path>` and drain the child's
/// stdout and stderr through two prefixing pump tasks.
///
/// The child starts in its own process group so [`terminate_with_grace`] can
/// target it precisely, and with `kill_on_drop` so a dropped handle can never
/// leave it running. When `log_path` is given, each pump also appends the raw,
/// unprefixed record it relays.
///
/// The returned join handles resolve once their pipe reaches EOF, so the caller
/// can [`drain_pumps`] them to be sure no output is lost.
pub async fn spawn_with_pumps(
    python_exe: &str,
    spec_path: &Path,
    attempt: &str,
    log_path: Option<&Path>,
) -> io::Result<(tokio::process::Child, Vec<tokio::task::JoinHandle<()>>)> {
    let (child, stdout, stderr) = spawn_child_pipes(python_exe, spec_path)?;

    // Each pump owns its own append handle to the log: the two streams are
    // independent writers and must not share a borrow.
    let attempt_out = attempt.to_string();
    let attempt_err = attempt.to_string();
    let mut out_log = open_log(log_path);
    let mut err_log = open_log(log_path);
    let pumps = vec![
        tokio::spawn(async move {
            let log = out_log.as_mut().map(|f| f as &mut (dyn std::io::Write + Send));
            let mut out = std::io::stdout();
            pump_prefixed(stdout, &attempt_out, &mut out, log).await;
        }),
        tokio::spawn(async move {
            let log = err_log.as_mut().map(|f| f as &mut (dyn std::io::Write + Send));
            let mut out = std::io::stdout();
            pump_prefixed(stderr, &attempt_err, &mut out, log).await;
        }),
    ];
    Ok((child, pumps))
}

/// Spawn the child and take its piped streams, without starting any pumps.
///
/// Split out from [`spawn_with_pumps`] so the pipes can be driven directly.
fn spawn_child_pipes(
    python_exe: &str,
    spec_path: &Path,
) -> io::Result<(tokio::process::Child, tokio::process::ChildStdout, tokio::process::ChildStderr)> {
    let mut command = tokio::process::Command::new(python_exe);
    command
        .arg("-m")
        .arg("gremlins.spawn.child")
        .arg(spec_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child stderr was not piped"))?;
    Ok((child, stdout, stderr))
}

/// Await every pump to completion, so the child's output is fully relayed
/// before the caller moves on. Handles that were aborted resolve immediately.
pub async fn drain_pumps(pumps: Vec<tokio::task::JoinHandle<()>>) {
    for pump in pumps {
        let _ = pump.await;
    }
}

/// Open an append-mode log sink, or `None` when there is no path or it cannot
/// be opened: a missing log must never fail the child run.
fn open_log(path: Option<&Path>) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path?)
        .ok()
}

/// Wait for `child`, terminating it with [`terminate_with_grace`] if `timeout_s`
/// elapses first.
///
/// `child_key` is carried only so the error can name the child.
pub async fn wait_child_proc(
    child: &mut tokio::process::Child,
    timeout_s: Option<f64>,
    child_key: &str,
) -> Result<std::process::ExitStatus, WaitChildError> {
    let io_error = |err: io::Error| WaitChildError {
        kind: WaitChildErrorKind::Io(err),
        child_key: child_key.to_string(),
        timeout_s,
    };

    let Some(seconds) = timeout_s else {
        return child.wait().await.map_err(io_error);
    };

    let deadline = if seconds.is_finite() && seconds > 0.0 {
        Duration::from_secs_f64(seconds)
    } else {
        Duration::MAX
    };
    match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(err)) => Err(io_error(err)),
        Err(_elapsed) => {
            if let Some(pid) = child.id() {
                terminate_with_grace(pid, 10.0).await;
            }
            Err(WaitChildError {
                kind: WaitChildErrorKind::Timeout,
                child_key: child_key.to_string(),
                timeout_s,
            })
        }
    }
}

/// How [`wait_child_proc`] failed.
#[derive(Debug)]
pub enum WaitChildErrorKind {
    /// The wait itself failed — the child could not be reaped.
    Io(io::Error),
    /// The child outlived its timeout and was terminated.
    Timeout,
}

/// A failed [`wait_child_proc`], carrying the context a useful message needs.
#[derive(Debug)]
pub struct WaitChildError {
    pub kind: WaitChildErrorKind,
    pub child_key: String,
    pub timeout_s: Option<f64>,
}

impl std::fmt::Display for WaitChildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            WaitChildErrorKind::Io(err) => {
                write!(f, "failed waiting for child {:?}: {err}", self.child_key)
            }
            WaitChildErrorKind::Timeout => write!(
                f,
                "parallel child {:?} timed out after {}s",
                self.child_key,
                self.timeout_s.unwrap_or_default()
            ),
        }
    }
}

impl std::error::Error for WaitChildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            WaitChildErrorKind::Io(err) => Some(err),
            WaitChildErrorKind::Timeout => None,
        }
    }
}

/// Relay one child pipe to stdout and `log_file`, prefixing each stdout record
/// with `[prefix]`.
///
/// Reads in [`PUMP_CHUNK`] chunks and carries only the trailing partial record
/// across reads, so the prefix is never inserted mid-line. The remainder is
/// relayed at EOF, or once it outgrows [`MAX_PENDING_BYTES`]. A trailing bare
/// `\r` is held back too: it may be the first half of a `\r\n` split across a
/// read boundary.
pub(crate) async fn pump_prefixed(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    prefix: &str,
    mut stdout: impl std::io::Write,
    mut log_file: Option<&mut (dyn std::io::Write + Send)>,
) {
    let mut decoder = Utf8Decoder::default();
    let mut pending = String::new();
    let mut buf = [0u8; PUMP_CHUNK];

    loop {
        let read = match stream.read(&mut buf).await {
            Ok(0) => {
                pending.push_str(&decoder.decode(&[], true));
                emit_tail(prefix, pending, &mut stdout, &mut log_file);
                return;
            }
            Ok(n) => n,
            // A read error is terminal for this pipe; the child's exit status —
            // not its log — decides the outcome.
            Err(_) => return,
        };
        pending.push_str(&decoder.decode(&buf[..read], false));

        for record in drain_records(&mut pending) {
            emit_prefixed(prefix, &record, &mut stdout, &mut log_file);
        }
        if pending.len() > MAX_PENDING_BYTES {
            let record = format!("{}\n", std::mem::take(&mut pending));
            emit_prefixed(prefix, &record, &mut stdout, &mut log_file);
        }
    }
}

/// Emit the unterminated remainder: verbatim when it already ends in a carriage
/// return, otherwise with a newline appended.
fn emit_tail(
    prefix: &str,
    pending: String,
    stdout: &mut impl std::io::Write,
    log_file: &mut Option<&mut (dyn std::io::Write + Send)>,
) {
    if pending.is_empty() {
        return;
    }
    if pending.ends_with('\r') {
        emit_prefixed(prefix, &pending, stdout, log_file);
    } else {
        emit_prefixed(prefix, &format!("{pending}\n"), stdout, log_file);
    }
}

/// Write one prefixed record to stdout and, when configured, the log.
///
/// Failures on either sink are swallowed: a broken pipe must not kill the pump
/// and strand the child.
fn emit_prefixed(
    prefix: &str,
    record: &str,
    stdout: &mut impl std::io::Write,
    log_file: &mut Option<&mut (dyn std::io::Write + Send)>,
) {
    let _ = write!(stdout, "[{prefix}] {record}");
    let _ = stdout.flush();
    if let Some(sink) = log_file.as_deref_mut() {
        let _ = sink.write_all(record.as_bytes());
        let _ = sink.flush();
    }
}

/// Move every complete record out of `pending`, leaving only the unterminated
/// remainder. Each record includes its `\r\n`, `\r`, or `\n` terminator.
fn drain_records(pending: &mut String) -> Vec<String> {
    let mut records = Vec::new();
    let mut start = 0;
    while let Some(end) = next_terminated_record(&pending[start..]) {
        records.push(pending[start..start + end].to_string());
        start += end;
    }
    if start > 0 {
        pending.drain(..start);
    }
    records
}

/// Length of the first terminated record in `s`, terminator included.
///
/// `None` means `s` holds only an unterminated remainder — or ends in a lone
/// `\r`, which may yet prove to be the first half of a `\r\n` split across a
/// read boundary, so it too must be held back.
fn next_terminated_record(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return Some(i + 1),
            b'\r' if i + 1 < bytes.len() => {
                return Some(if bytes[i + 1] == b'\n' { i + 2 } else { i + 1 });
            }
            b'\r' => return None,
            _ => i += 1,
        }
    }
    None
}

/// A minimal incremental UTF-8 decoder: multibyte sequences split across reads
/// are carried forward, and malformed bytes become U+FFFD — matching Python's
/// `codecs.getincrementaldecoder("utf-8")("replace")`.
#[derive(Default)]
struct Utf8Decoder {
    /// Bytes seen but not yet decodable on their own.
    carry: Vec<u8>,
}

impl Utf8Decoder {
    fn decode(&mut self, chunk: &[u8], eof: bool) -> String {
        self.carry.extend_from_slice(chunk);
        match std::str::from_utf8(&self.carry) {
            Ok(text) => {
                let text = text.to_string();
                self.carry.clear();
                text
            }
            Err(err) => {
                let valid = err.valid_up_to();
                let mut text = String::from_utf8(self.carry[..valid].to_vec())
                    .expect("valid_up_to marks a valid UTF-8 prefix");
                match err.error_len() {
                    // A malformed sequence: replace it and carry on past it.
                    Some(len) => {
                        text.push('\u{FFFD}');
                        self.carry.drain(..valid + len);
                    }
                    // A truncated tail: hold it, unless the stream is done.
                    None if eof => {
                        text.push('\u{FFFD}');
                        self.carry.clear();
                    }
                    None => {
                        self.carry.drain(..valid);
                    }
                }
                text
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_ok_success() {
        assert!(run_ok(&["true".to_string()], None).unwrap());
    }

    #[test]
    fn test_run_ok_failure() {
        assert!(!run_ok(&["false".to_string()], None).unwrap());
    }

    #[test]
    fn test_run_ok_missing_command() {
        let err = run_ok(&["_nonexistent_command_xyzzy_".to_string()], None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_run_ok_empty_cmd() {
        let err = run_ok(&[], None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_run_ok_with_cwd() {
        assert!(run_ok(&["pwd".to_string()], Some(Path::new("/"))).unwrap());
    }

    #[test]
    fn test_run_success() {
        let r = run(&["true".to_string()], None, false, None).unwrap();
        assert_eq!(r.returncode, 0);
    }

    #[test]
    fn test_run_failure_no_check() {
        let r = run(&["false".to_string()], None, false, None).unwrap();
        assert_ne!(r.returncode, 0);
    }

    #[test]
    fn test_run_check_raises() {
        let err = run(&["false".to_string()], None, true, None).unwrap_err();
        match err {
            ProcError::CalledProcessError(..) => {}
            _ => panic!("expected CalledProcessError, got {err}"),
        }
    }

    #[test]
    fn test_run_captures_stdout() {
        let r = run(
            &["echo".to_string(), "hello".to_string()],
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "hello");
    }

    #[test]
    fn test_run_captures_stderr() {
        let r = run(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo err >&2".to_string(),
            ],
            None,
            false,
            None,
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&r.stderr).contains("err"));
    }

    #[test]
    fn test_run_timeout() {
        let err = run(
            &["sleep".to_string(), "10".to_string()],
            None,
            false,
            Some(0.05),
        )
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(..) => {}
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[test]
    fn test_run_timeout_accumulates_partial_output() {
        let err = run(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo start; sleep 10".to_string(),
            ],
            None,
            false,
            Some(0.1),
        )
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(_, stdout, _) => {
                assert!(
                    stdout.windows(5).any(|w| w == b"start"),
                    "partial stdout should contain 'start', got: {:?}",
                    String::from_utf8_lossy(&stdout)
                );
            }
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[test]
    fn test_run_timeout_large_output() {
        // Generate output larger than the OS pipe buffer (~64KB) under a
        // generous timeout to verify pipes are drained concurrently.
        let err = run(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "dd if=/dev/zero bs=131072 count=1 2>/dev/null; sleep 10".to_string(),
            ],
            None,
            false,
            Some(0.2),
        )
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(_, stdout, _) => {
                assert!(!stdout.is_empty(), "large output should not block");
            }
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[test]
    fn test_run_invalid_timeout_negative() {
        let err = run(&["true".to_string()], None, false, Some(-1.0)).unwrap_err();
        match err {
            ProcError::InvalidTimeout(_) => {}
            _ => panic!("expected InvalidTimeout, got {err}"),
        }
    }

    #[test]
    fn test_run_invalid_timeout_nan() {
        let err = run(&["true".to_string()], None, false, Some(f64::NAN)).unwrap_err();
        match err {
            ProcError::InvalidTimeout(_) => {}
            _ => panic!("expected InvalidTimeout, got {err}"),
        }
    }

    #[test]
    fn test_run_invalid_timeout_infinite() {
        let err = run(&["true".to_string()], None, false, Some(f64::INFINITY)).unwrap_err();
        match err {
            ProcError::InvalidTimeout(_) => {}
            _ => panic!("expected InvalidTimeout, got {err}"),
        }
    }

    #[test]
    fn test_run_empty_cmd() {
        let err = run(&[], None, false, None).unwrap_err();
        match err {
            ProcError::EmptyCommand => {}
            _ => panic!("expected EmptyCommand, got {err}"),
        }
    }

    #[test]
    fn test_run_with_cwd() {
        let r = run(&["pwd".to_string()], Some(Path::new("/")), false, None).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "/");
    }

    #[test]
    fn test_run_quiet_success() {
        let r = run_quiet(&["true".to_string()], None).unwrap();
        assert_eq!(r.returncode, 0);
    }

    #[test]
    fn test_run_quiet_failure() {
        let r = run_quiet(&["false".to_string()], None).unwrap();
        assert_ne!(r.returncode, 0);
    }

    #[test]
    fn test_run_quiet_missing_command() {
        let err = run_quiet(&["_nonexistent_command_xyzzy_".to_string()], None).unwrap_err();
        match err {
            ProcError::Io(e) => {
                #[cfg(unix)]
                assert_eq!(e.kind(), io::ErrorKind::NotFound);
            }
            _ => panic!("expected Io error, got {err}"),
        }
    }

    #[test]
    fn test_run_quiet_empty_cmd() {
        let err = run_quiet(&[], None).unwrap_err();
        match err {
            ProcError::EmptyCommand => {}
            _ => panic!("expected EmptyCommand, got {err}"),
        }
    }

    #[test]
    fn test_run_quiet_with_cwd() {
        let r = run_quiet(
            &[
                "sh".to_string(),
                "-c".to_string(),
                r#"test "$(pwd)" = /"#.to_string(),
            ],
            Some(Path::new("/")),
        )
        .unwrap();
        assert_eq!(r.returncode, 0);
    }

    #[test]
    fn test_run_quiet_stdout_not_captured() {
        let r = run_quiet(&["echo".to_string(), "hello".to_string()], None).unwrap();
        assert_eq!(r.returncode, 0);
        assert!(r.stdout.is_empty());
    }

    #[test]
    fn test_run_missing_command() {
        let err = run(
            &["_nonexistent_command_xyzzy_".to_string()],
            None,
            false,
            None,
        )
        .unwrap_err();
        match err {
            ProcError::Io(e) => {
                #[cfg(unix)]
                assert_eq!(e.kind(), io::ErrorKind::NotFound);
            }
            _ => panic!("expected Io error, got {err}"),
        }
    }

    #[test]
    fn test_run_or_raise_success() {
        let out = run_or_raise(&["echo".to_string(), "hello".to_string()], None).unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn test_run_or_raise_trailing_newline_stripped() {
        let out = run_or_raise(&["printf".to_string(), "hello\n".to_string()], None).unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn test_run_or_raise_nonzero_exit() {
        let err = run_or_raise(&["false".to_string()], None).unwrap_err();
        match err {
            ProcError::CalledProcessError(..) => {}
            _ => panic!("expected CalledProcessError, got {err}"),
        }
    }

    #[test]
    fn test_run_or_raise_missing_command() {
        let err = run_or_raise(&["_nonexistent_command_xyzzy_".to_string()], None).unwrap_err();
        match err {
            ProcError::Io(e) => {
                #[cfg(unix)]
                assert_eq!(e.kind(), io::ErrorKind::NotFound);
            }
            _ => panic!("expected Io error, got {err}"),
        }
    }

    #[test]
    fn test_run_or_raise_empty_cmd() {
        let err = run_or_raise(&[], None).unwrap_err();
        match err {
            ProcError::EmptyCommand => {}
            _ => panic!("expected EmptyCommand, got {err}"),
        }
    }

    #[test]
    fn test_run_or_raise_with_cwd() {
        let out = run_or_raise(&["pwd".to_string()], Some(Path::new("/"))).unwrap();
        assert_eq!(out, "/");
    }

    // -- run_ok_async tests --

    #[tokio::test]
    async fn test_run_ok_async_success() {
        assert!(run_ok_async(&["true".to_string()], None).await.unwrap());
    }

    #[tokio::test]
    async fn test_run_ok_async_failure() {
        assert!(!run_ok_async(&["false".to_string()], None).await.unwrap());
    }

    #[tokio::test]
    async fn test_run_ok_async_missing_command() {
        let err = run_ok_async(&["_nonexistent_command_xyzzy_".to_string()], None)
            .await
            .unwrap_err();
        #[cfg(unix)]
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn test_run_ok_async_empty_cmd() {
        let err = run_ok_async(&[], None).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn test_run_ok_async_with_cwd() {
        assert!(run_ok_async(
            &[
                "sh".to_string(),
                "-c".to_string(),
                r#"test "$(pwd)" = /"#.to_string(),
            ],
            Some(Path::new("/")),
        )
        .await
        .unwrap());
    }

    // -- run_async tests --

    #[tokio::test]
    async fn test_run_async_success() {
        let r = run_async(&["true".to_string()], None, false, None, true, None)
            .await
            .unwrap();
        assert_eq!(r.returncode, 0);
    }

    #[tokio::test]
    async fn test_run_async_nonzero_exit() {
        let r = run_async(&["false".to_string()], None, false, None, true, None)
            .await
            .unwrap();
        assert_ne!(r.returncode, 0);
    }

    #[tokio::test]
    async fn test_run_async_check_raises() {
        let err = run_async(&["false".to_string()], None, true, None, true, None)
            .await
            .unwrap_err();
        match err {
            ProcError::CalledProcessError(..) => {}
            _ => panic!("expected CalledProcessError, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_captures_stdout() {
        let r = run_async(
            &["echo".to_string(), "hello".to_string()],
            None,
            false,
            None,
            true,
            None,
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "hello");
    }

    #[tokio::test]
    async fn test_run_async_captures_stderr() {
        let r = run_async(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo err >&2".to_string(),
            ],
            None,
            false,
            None,
            true,
            None,
        )
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&r.stderr).contains("err"));
    }

    #[tokio::test]
    async fn test_run_async_timeout() {
        let err = run_async(
            &["sleep".to_string(), "10".to_string()],
            None,
            false,
            Some(0.05),
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(..) => {}
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_empty_cmd() {
        let err = run_async(&[], None, false, None, true, None)
            .await
            .unwrap_err();
        match err {
            ProcError::EmptyCommand => {}
            _ => panic!("expected EmptyCommand, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_invalid_timeout() {
        let err = run_async(&["true".to_string()], None, false, Some(-1.0), true, None)
            .await
            .unwrap_err();
        match err {
            ProcError::InvalidTimeout(_) => {}
            _ => panic!("expected InvalidTimeout, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_invalid_timeout_nan() {
        let err = run_async(
            &["true".to_string()],
            None,
            false,
            Some(f64::NAN),
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::InvalidTimeout(_) => {}
            _ => panic!("expected InvalidTimeout, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_invalid_timeout_infinite() {
        let err = run_async(
            &["true".to_string()],
            None,
            false,
            Some(f64::INFINITY),
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::InvalidTimeout(_) => {}
            _ => panic!("expected InvalidTimeout, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_timeout_accumulates_partial_output() {
        let err = run_async(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo start; sleep 10".to_string(),
            ],
            None,
            false,
            Some(0.1),
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(_, stdout, _) => {
                assert!(
                    stdout.windows(5).any(|w| w == b"start"),
                    "partial stdout should contain 'start', got: {:?}",
                    String::from_utf8_lossy(&stdout)
                );
            }
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_timeout_large_output() {
        let err = run_async(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "dd if=/dev/zero bs=131072 count=1 2>/dev/null; sleep 10".to_string(),
            ],
            None,
            false,
            Some(0.2),
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(_, stdout, _) => {
                assert!(!stdout.is_empty(), "large output should not block");
            }
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_with_cwd() {
        let r = run_async(
            &["pwd".to_string()],
            Some(Path::new("/")),
            false,
            None,
            true,
            None,
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "/");
    }

    #[tokio::test]
    async fn test_run_async_missing_command() {
        let err = run_async(
            &["_nonexistent_command_xyzzy_".to_string()],
            None,
            false,
            None,
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::Io(e) => {
                #[cfg(unix)]
                assert_eq!(e.kind(), io::ErrorKind::NotFound);
            }
            _ => panic!("expected Io error, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_async_cancel_kills_process() {
        let handle = tokio::spawn(async {
            run_async(
                &["sleep".to_string(), "10".to_string()],
                None,
                false,
                None,
                true,
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let result = handle.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_async_cancel_kills_process_group() {
        let handle = tokio::spawn(async {
            run_async(
                &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "sleep 10 & sleep 10".to_string(),
                ],
                None,
                false,
                None,
                true,
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let result = handle.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_async_text_false_returns_bytes() {
        let r = run_async(
            &["echo".to_string(), "hello".to_string()],
            None,
            false,
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(&r.stdout, b"hello\n");
    }

    #[tokio::test]
    async fn test_run_async_text_true_returns_strings_in_error() {
        let err = run_async(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo hi; exit 1".to_string(),
            ],
            None,
            true,
            None,
            true,
            None,
        )
        .await
        .unwrap_err();
        match err {
            ProcError::CalledProcessError(_, stdout, _) => {
                assert!(stdout.windows(2).any(|w| w == b"hi"));
            }
            _ => panic!("expected CalledProcessError, got {err}"),
        }
    }

    // -- terminate_with_grace tests --

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminate_with_grace_sigterm_kills() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        // The process should die from SIGTERM within the grace window.
        terminate_with_grace(pid, 0.5).await;
        let status = child.wait().await.unwrap();
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminate_with_grace_already_dead() {
        let mut child = tokio::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let _ = child.wait().await;
        // Should not panic or hang.
        terminate_with_grace(pid, 0.1).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminate_with_grace_grace_expires_sigkill() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        // Very short grace forces SIGKILL path.
        terminate_with_grace(pid, 0.05).await;
        let status = child.wait().await.unwrap();
        // SIGKILL produces a signal exit, not a normal exit.
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn test_terminate_with_grace_blocking_sigterm_kills() {
        // The blocking variant must work without any async runtime context.
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        terminate_with_grace_blocking(pid, 0.5);
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn test_terminate_with_grace_blocking_escalates_to_sigkill() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        terminate_with_grace_blocking(pid, 0.05);
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn test_terminate_with_grace_blocking_already_dead() {
        let mut child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let _ = child.wait();
        // Must not panic or hang.
        terminate_with_grace_blocking(pid, 0.1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_terminate_with_grace_cancellation_safety() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        // Spawn terminate_with_grace and cancel the outer task.
        let handle = tokio::spawn(async move {
            terminate_with_grace(pid, 0.5).await;
        });
        // Wait long enough for the inner kill-task to start and send SIGTERM.
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.abort();
        let _ = handle.await;
        // The child should still be killed even though we cancelled.
        let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }

    // -- run_shell_async tests --

    #[tokio::test]
    async fn test_run_shell_async_success() {
        let r = run_shell_async("true", None, None, None).await.unwrap();
        assert_eq!(r.returncode, 0);
    }

    #[tokio::test]
    async fn test_run_shell_async_captures_stdout() {
        let r = run_shell_async("echo hello", None, None, None)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "hello");
    }

    #[tokio::test]
    async fn test_run_shell_async_timeout() {
        let err = run_shell_async("sleep 10", None, None, Some(0.05))
            .await
            .unwrap_err();
        match err {
            ProcError::TimeoutExpired(..) => {}
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_shell_async_timeout_kills_grandchildren() {
        let err = run_shell_async("sleep 60 & sleep 60", None, None, Some(0.1))
            .await
            .unwrap_err();
        match err {
            ProcError::TimeoutExpired(..) => {}
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_shell_async_large_output() {
        let err = run_shell_async(
            "dd if=/dev/zero bs=131072 count=1 2>/dev/null; sleep 10",
            None,
            None,
            Some(0.2),
        )
        .await
        .unwrap_err();
        match err {
            ProcError::TimeoutExpired(_, stdout, _) => {
                assert!(!stdout.is_empty(), "large output should not block");
            }
            _ => panic!("expected TimeoutExpired, got {err}"),
        }
    }

    #[tokio::test]
    async fn test_run_shell_async_cancel() {
        let handle = tokio::spawn(async { run_shell_async("sleep 10", None, None, None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let result = handle.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_shell_async_with_env() {
        let r = run_shell_async(
            "echo $FOO",
            None,
            Some(&HashMap::from([("FOO".to_string(), "bar".to_string())])),
            None,
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&r.stdout).trim(), "bar");
    }

    #[tokio::test]
    async fn test_run_shell_async_empty_cmd() {
        let err = run_shell_async("", None, None, None).await.unwrap_err();
        match err {
            ProcError::EmptyCommand => {}
            _ => panic!("expected EmptyCommand, got {err}"),
        }
    }

    // -- pump_prefixed tests --

    /// Feed `chunks` through a duplex pipe, then run the pump and return the
    /// prefixed stdout it produced and the raw records it wrote to its log.
    async fn pump(chunks: &[&[u8]], prefix: &str) -> (String, Vec<u8>) {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(MAX_PENDING_BYTES * 2);
        for chunk in chunks {
            writer.write_all(chunk).await.unwrap();
        }
        drop(writer);
        let mut stdout = Vec::new();
        let mut log = Vec::new();
        pump_prefixed(reader, prefix, &mut stdout, Some(&mut log)).await;
        (String::from_utf8(stdout).unwrap(), log)
    }

    #[tokio::test]
    async fn test_pump_prefixed_basic() {
        let (stdout, log) = pump(&[b"one\ntwo\n"], "p").await;
        assert_eq!(stdout, "[p] one\n[p] two\n");
        assert_eq!(log, b"one\ntwo\n");
    }

    #[tokio::test]
    async fn test_pump_prefixed_partial_line_flushed_at_eof() {
        let (stdout, log) = pump(&[b"no newline"], "p").await;
        assert_eq!(stdout, "[p] no newline\n");
        assert_eq!(log, b"no newline\n");
    }

    #[tokio::test]
    async fn test_pump_prefixed_crlf_is_one_record() {
        let (stdout, log) = pump(&[b"one\rtwo\r\n"], "p").await;
        assert_eq!(stdout, "[p] one\r[p] two\r\n");
        assert_eq!(log, b"one\rtwo\r\n");
    }

    #[tokio::test]
    async fn test_pump_prefixed_holds_terminal_cr_across_reads() {
        // The `\r` must not be emitted on its own: it is the first half of a
        // CRLF split across the read boundary.
        let (stdout, log) = pump(&[b"one\r", b"\ntwo\n"], "p").await;
        assert_eq!(stdout, "[p] one\r\n[p] two\n");
        assert_eq!(log, b"one\r\ntwo\n");
    }

    #[tokio::test]
    async fn test_pump_prefixed_flushes_bare_cr_at_eof() {
        let (stdout, log) = pump(&[b"one\r"], "p").await;
        assert_eq!(stdout, "[p] one\r");
        assert_eq!(log, b"one\r");
    }

    #[tokio::test]
    async fn test_pump_prefixed_joins_multibyte_across_reads() {
        let (stdout, log) = pump(&[b"caf\xc3", b"\xa9\n"], "p").await;
        assert_eq!(stdout, "[p] caf\u{e9}\n");
        assert_eq!(log, "caf\u{e9}\n".as_bytes());
    }

    #[tokio::test]
    async fn test_pump_prefixed_flushes_oversized_pending() {
        let blob = vec![b'x'; MAX_PENDING_BYTES + 10];
        let (stdout, log) = pump(&[&blob], "p").await;
        assert_eq!(log.len(), blob.len() + 1);
        assert_eq!(&log[..blob.len()], blob.as_slice());
        assert_eq!(log[blob.len()], b'\n');
        assert_eq!(stdout.len(), blob.len() + 1 + "[p] ".len());
    }

    #[test]
    fn test_next_terminated_record() {
        assert_eq!(next_terminated_record("ab\ncd"), Some(3));
        assert_eq!(next_terminated_record("ab\r\ncd"), Some(4));
        assert_eq!(next_terminated_record("ab\rcd"), Some(3));
        assert_eq!(next_terminated_record("ab\r"), None);
        assert_eq!(next_terminated_record("abc"), None);
    }

    #[test]
    fn test_drain_records_holds_partial() {
        let mut pending = String::from("one\ntwo\r\nthree");
        assert_eq!(drain_records(&mut pending), vec!["one\n", "two\r\n"]);
        assert_eq!(pending, "three");
    }

    #[test]
    fn test_utf8_decoder_replaces_invalid() {
        let mut decoder = Utf8Decoder::default();
        assert_eq!(decoder.decode(b"ok\xff", false), "ok\u{FFFD}");
    }

    // -- spawn_with_pumps / wait_child_proc tests --

    #[tokio::test]
    async fn test_spawn_with_pumps_relays_output() {
        // `echo` stands in for the interpreter: it prints its argv and exits.
        let (mut child, pumps) =
            spawn_with_pumps("echo", Path::new("/tmp/spec.json"), "attempt", None)
                .await
                .unwrap();
        drain_pumps(pumps).await;
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn test_wait_child_proc_no_timeout() {
        let mut child = tokio::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = wait_child_proc(&mut child, None, "k").await.unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn test_wait_child_proc_timeout_terminates() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("10")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let err = wait_child_proc(&mut child, Some(0.1), "k").await.unwrap_err();
        assert!(matches!(err.kind, WaitChildErrorKind::Timeout));
        assert!(err.to_string().contains("timed out"));
        // The child was terminated, so reaping it completes promptly.
        let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("child should have been terminated")
            .unwrap();
        assert!(!status.success());
    }
}
