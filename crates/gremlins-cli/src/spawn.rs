use std::fs;
use std::process::{Command, Stdio};

use gremlins::config;

/// Spawn a detached child process that runs the gremlin pipeline.
///
/// The child re-invokes the current binary with the `spawn` subcommand.
/// stdin is /dev/null; stdout and stderr are appended to the gremlin's log.
pub(crate) fn spawn_gremlin(id: &str, resume_from: Option<&str>) -> Result<(), String> {
    let log_path = config::state_root().join(id).join("log");

    let log_file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open log: {e}"))?;

    let stdout_file = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone log handle: {e}"))?;

    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;

    let mut cmd = Command::new(current_exe);
    cmd.arg("spawn").arg(id);
    if let Some(stage) = resume_from {
        cmd.arg("--resume-from").arg(stage);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| format!("failed to spawn gremlin: {e}"))?;

    Ok(())
}
