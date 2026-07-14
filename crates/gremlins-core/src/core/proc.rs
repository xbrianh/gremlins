use std::io;
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Return type for `run`.
#[derive(Debug)]
pub struct ProcResult {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Error type for `run`.
#[derive(Debug)]
pub enum ProcError {
    CalledProcessError(i32, String, String),
    TimeoutExpired(f64, String, String),
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

fn exit_code(status: &std::process::ExitStatus) -> i32 {
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
        stdout: String::new(),
        stderr: String::new(),
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
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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
                stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
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
            Err(ProcError::TimeoutExpired(
                timeout_s,
                String::from_utf8_lossy(&stdout_buf).into_owned(),
                String::from_utf8_lossy(&stderr_buf).into_owned(),
            ))
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
        assert_eq!(r.stdout.trim(), "hello");
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
        assert!(r.stderr.contains("err"));
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
                    stdout.contains("start"),
                    "partial stdout should contain 'start', got: {stdout}"
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
        assert_eq!(r.stdout.trim(), "/");
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
}
