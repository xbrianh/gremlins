use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;

#[derive(Debug)]
pub struct ProcResult {
    pub returncode: i32,
    pub stdout: Option<Vec<u8>>,
    pub stderr: Option<Vec<u8>>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum ProcError {
    CalledProcessError {
        returncode: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TimeoutExpired {
        cmd: Vec<String>,
        timeout: f64,
    },
    Io(std::io::Error),
    ProcessLookup,
}

impl From<std::io::Error> for ProcError {
    fn from(e: std::io::Error) -> Self {
        ProcError::Io(e)
    }
}

impl std::fmt::Display for ProcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcError::CalledProcessError { returncode, .. } => {
                write!(f, "Command exited with status {}", returncode)
            }
            ProcError::TimeoutExpired { cmd, timeout } => {
                write!(f, "Command {:?} timed out after {}s", cmd, timeout)
            }
            ProcError::Io(e) => write!(f, "IO error: {}", e),
            ProcError::ProcessLookup => write!(f, "Process not found"),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn exit_status_to_rc(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        // Unix signal: return negative signal number
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            -(status.signal().unwrap_or(1) as i32)
        }
        #[cfg(not(unix))]
        {
            -1
        }
    })
}

/// Kills the process group on drop if not disarmed.
struct ChildGuard {
    pid: Option<i32>,
}

impl ChildGuard {
    fn new(pid: Option<i32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

// ---------------------------------------------------------------------------
// Sync spawn
// ---------------------------------------------------------------------------

pub fn spawn_sync(
    cmd: &[String],
    cwd: Option<&Path>,
    capture: bool,
    check: bool,
    timeout: Option<Duration>,
) -> Result<ProcResult, ProcError> {
    let mut command = std::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if capture {
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
    }

    let child = command.spawn()?;
    let pid = child.id() as i32;

    let output = if let Some(timeout) = timeout {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = child.wait_with_output();
            let _ = tx.send(());
            result
        });
        match rx.recv_timeout(timeout) {
            Ok(()) => handle.join().unwrap()?,
            Err(_timeout) => {
                let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
                let _ = handle.join();
                return Err(ProcError::TimeoutExpired {
                    cmd: cmd.to_vec(),
                    timeout: timeout.as_secs_f64(),
                });
            }
        }
    } else {
        child.wait_with_output()?
    };

    let rc = exit_status_to_rc(output.status);

    if check && rc != 0 {
        return Err(ProcError::CalledProcessError {
            returncode: rc,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }

    let stdout = if capture { Some(output.stdout) } else { None };
    let stderr = if capture { Some(output.stderr) } else { None };

    Ok(ProcResult {
        returncode: rc,
        stdout,
        stderr,
    })
}

// ---------------------------------------------------------------------------
// Async spawn (capturing)
// ---------------------------------------------------------------------------

pub async fn spawn_async(
    cmd: &[String],
    cwd: Option<&Path>,
    check: bool,
    timeout: Option<Duration>,
) -> Result<ProcResult, ProcError> {
    let mut command = tokio::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    unsafe {
        command.pre_exec(|| {
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }

    let mut child = Some(command.spawn()?);
    let pid = child.as_ref().and_then(|c| c.id()).map(|id| id as i32);
    let mut guard = ChildGuard::new(pid);

    let output = if let Some(timeout) = timeout {
        let child = child.take().unwrap();
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result?,
            Err(_elapsed) => {
                if let Some(pid) = pid {
                    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
                }
                return Err(ProcError::TimeoutExpired {
                    cmd: cmd.to_vec(),
                    timeout: timeout.as_secs_f64(),
                });
            }
        }
    } else {
        child.take().unwrap().wait_with_output().await?
    };

    guard.disarm();
    let rc = exit_status_to_rc(output.status);

    if check && rc != 0 {
        return Err(ProcError::CalledProcessError {
            returncode: rc,
            stdout: output.stdout,
            stderr: output.stderr,
        });
    }

    Ok(ProcResult {
        returncode: rc,
        stdout: Some(output.stdout),
        stderr: Some(output.stderr),
    })
}

// ---------------------------------------------------------------------------
// Async spawn (null io) — used by run_ok_async / run_quiet_async
// ---------------------------------------------------------------------------

pub async fn spawn_async_nullio(
    cmd: &[String],
    cwd: Option<&Path>,
) -> Result<Option<i32>, ProcError> {
    let mut command = tokio::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command.kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    unsafe {
        command.pre_exec(|| {
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let pid = child.id().map(|id| id as i32);
    let mut guard = ChildGuard::new(pid);

    let status = child.wait().await?;
    guard.disarm();
    Ok(status.code())
}

// ---------------------------------------------------------------------------
// Shell async
// ---------------------------------------------------------------------------

pub async fn spawn_shell_async(
    cmd: &str,
    cwd: Option<&Path>,
    env: Option<&HashMap<String, String>>,
    timeout: Option<Duration>,
) -> Result<ProcResult, ProcError> {
    let shell = if cfg!(target_os = "windows") {
        "cmd.exe"
    } else {
        "/bin/sh"
    };
    let shell_arg = if cfg!(target_os = "windows") {
        "/C"
    } else {
        "-c"
    };

    let mut command = tokio::process::Command::new(shell);
    command.arg(shell_arg).arg(cmd);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = env {
        command.env_clear();
        for (k, v) in env {
            command.env(k, v);
        }
    }
    unsafe {
        command.pre_exec(|| {
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let pid = child.id().map(|id| id as i32);
    let mut guard = ChildGuard::new(pid);

    let mut child_stdout = child.stdout.take().unwrap();
    let mut child_stderr = child.stderr.take().unwrap();

    let read_stdout: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>> =
        tokio::spawn(async move {
            let mut buf = vec![];
            child_stdout.read_to_end(&mut buf).await?;
            Ok(buf)
        });
    let read_stderr: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>> =
        tokio::spawn(async move {
            let mut buf = vec![];
            child_stderr.read_to_end(&mut buf).await?;
            Ok(buf)
        });

    if let Some(timeout) = timeout {
        let sleep = tokio::time::sleep(timeout);
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                // Child exited; drain pipes with a secondary timeout so a
                // background descendant holding a pipe open cannot extend us
                // past the original deadline.
                let drain = async {
                    let stdout = read_stdout.await.unwrap()?;
                    let stderr = read_stderr.await.unwrap()?;
                    Ok::<_, ProcError>((stdout, stderr))
                };
                match tokio::time::timeout(timeout, drain).await {
                    Ok(Ok((stdout, stderr))) => {
                        guard.disarm();
                        let rc = exit_status_to_rc(status);
                        Ok(ProcResult { returncode: rc, stdout: Some(stdout), stderr: Some(stderr) })
                    }
                    Ok(Err(e)) => Err(e),
                    Err(_) => {
                        // Pipe drain timed out — kill and return what we have
                        if let Some(pid) = pid {
                            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
                        }
                        let _ = child.wait().await;
                        guard.disarm();
                        let stderr_s = format!("timed out after {}s\n", timeout.as_secs_f64());
                        Ok(ProcResult {
                            returncode: 124,
                            stdout: Some(vec![]),
                            stderr: Some(stderr_s.into_bytes()),
                        })
                    }
                }
            }
            _ = sleep => {
                if let Some(pid) = pid {
                    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
                }
                let _ = child.wait().await;
                // Drain whatever is left with a short grace timeout
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    let _ = read_stdout.await;
                    let _ = read_stderr.await;
                }).await;
                guard.disarm();
                let stderr_s = format!("timed out after {}s\n", timeout.as_secs_f64());
                Ok(ProcResult {
                    returncode: 124,
                    stdout: Some(vec![]),
                    stderr: Some(stderr_s.into_bytes()),
                })
            }
        }
    } else {
        let status = child.wait().await?;
        let stdout = read_stdout.await.unwrap()?;
        let stderr = read_stderr.await.unwrap()?;
        guard.disarm();
        let rc = exit_status_to_rc(status);
        Ok(ProcResult {
            returncode: rc,
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }
}

// ---------------------------------------------------------------------------
// kill_process_group
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn kill_process_group(pid: i32, sig: Signal) -> Result<(), ProcError> {
    match killpg(Pid::from_raw(pid), sig) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Err(ProcError::ProcessLookup),
        Err(nix::errno::Errno::EPERM) => Ok(()),
        Err(e) => Err(ProcError::Io(std::io::Error::from_raw_os_error(e as i32))),
    }
}

// ---------------------------------------------------------------------------
// terminate_with_grace — only signals the process group; caller must reap
// ---------------------------------------------------------------------------

pub async fn terminate_with_grace(pid: i32, grace_s: f64) -> Result<(), ProcError> {
    match killpg(Pid::from_raw(pid), Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return Ok(()),
        Err(nix::errno::Errno::EPERM) => return Ok(()),
        Err(e) => return Err(ProcError::Io(std::io::Error::from_raw_os_error(e as i32))),
    }

    tokio::time::sleep(Duration::from_secs_f64(grace_s)).await;

    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_sync_success() {
        let result = spawn_sync(&["true".into()], None, true, false, None).unwrap();
        assert_eq!(result.returncode, 0);
    }

    #[test]
    fn test_spawn_sync_failure() {
        let result = spawn_sync(&["false".into()], None, true, false, None).unwrap();
        assert_ne!(result.returncode, 0);
    }

    #[test]
    fn test_spawn_sync_check_raises() {
        let result = spawn_sync(&["false".into()], None, true, true, None);
        assert!(matches!(result, Err(ProcError::CalledProcessError { .. })));
    }

    #[test]
    fn test_spawn_sync_captures_stdout() {
        let result = spawn_sync(&["echo".into(), "hello".into()], None, true, false, None).unwrap();
        let stdout = String::from_utf8_lossy(result.stdout.as_ref().unwrap());
        assert!(stdout.contains("hello"));
    }

    #[test]
    fn test_spawn_sync_timeout() {
        let result = spawn_sync(
            &["sleep".into(), "10".into()],
            None,
            true,
            false,
            Some(Duration::from_millis(50)),
        );
        assert!(matches!(result, Err(ProcError::TimeoutExpired { .. })));
    }

    #[test]
    fn test_kill_process_group_esrch() {
        let result = kill_process_group(99999, Signal::SIGTERM);
        assert!(matches!(result, Err(ProcError::ProcessLookup)));
    }

    #[tokio::test]
    async fn test_spawn_async_success() {
        let result = spawn_async(&["true".into()], None, false, None)
            .await
            .unwrap();
        assert_eq!(result.returncode, 0);
    }

    #[tokio::test]
    async fn test_spawn_async_timeout() {
        let result = spawn_async(
            &["sleep".into(), "10".into()],
            None,
            false,
            Some(Duration::from_millis(50)),
        )
        .await;
        assert!(matches!(result, Err(ProcError::TimeoutExpired { .. })));
    }

    #[tokio::test]
    async fn test_spawn_shell_async_timeout_returns_124() {
        let result = spawn_shell_async("sleep 10", None, None, Some(Duration::from_millis(50)))
            .await
            .unwrap();
        assert_eq!(result.returncode, 124);
    }

    #[tokio::test]
    async fn test_terminate_with_grace_smoke() {
        let mut command = tokio::process::Command::new("sleep");
        command.arg("60");
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        command.kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap() as i32;

        terminate_with_grace(pid, 0.1).await.unwrap();
        // Reap the child ourselves since terminate_with_grace only signals
        let _ = child.wait().await;
    }
}