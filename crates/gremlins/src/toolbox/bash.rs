use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::{ChildStderr, ChildStdout, Command};

use crate::clients::format::format_tool_call_parts;
use crate::clients::tools::{parse_args, project_root, req_str, AgentLoopRuntime, ToolHandler};

const BASH_TIMEOUT_SECS: u64 = 120;

pub(crate) struct BashTool;

#[async_trait]
impl ToolHandler for BashTool {
    async fn invoke(&self, id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        bash_invoke(ctx, args_json, id).await
    }
}

/// Kills the command's process group when the tool future is dropped or the
/// timeout elapses. `tokio::process::Child::kill_on_drop` only reaches the
/// direct `bash` process; a process group guard is what stops descendants
/// spawned by the shell (`sleep 3600 & wait`).
#[cfg(unix)]
pub(super) struct ProcessGroupGuard {
    pgid: Option<u32>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    pub(super) fn new(pgid: Option<u32>) -> Self {
        Self { pgid }
    }

    pub(super) fn disarm(&mut self) {
        self.pgid = None;
    }

    pub(super) fn kill(&mut self) {
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
        }
    }
}

/// Cursor state for [`emit_live_lines`]: `scan` marks the start of the line
/// currently being assembled and `searched` how far into the buffer the
/// previous call already looked for newlines.
pub(super) struct LineScanner {
    scan: usize,
    searched: usize,
}

impl LineScanner {
    pub(super) fn new() -> Self {
        Self {
            scan: 0,
            searched: 0,
        }
    }
}

pub(super) async fn emit_live_lines(
    id: &str,
    name: &str,
    call: &str,
    ctx: &AgentLoopRuntime,
    bytes: &[u8],
    scanner: &mut LineScanner,
    flush: bool,
) {
    // `scanner.scan` marks the start of the line currently being assembled;
    // `scanner.searched` is how far into `bytes` the previous call already
    // looked for newlines. Each call only searches the region appended since
    // the last call, so a long newline-free run costs O(new bytes) instead of
    // re-scanning the whole accumulated buffer on every 8 KiB read (the old
    // `bytes[scan..]` scan was O(n²) on newline-free output).
    //
    // Completed lines are coalesced into a single chunk so a busy command
    // that emits hundreds of lines per read costs one channel event rather
    // than one event per line — per-line chunks would crowd the terminal
    // `ToolResult`/`TurnComplete`/`Done` control events out of the bounded
    // chat channel. Downstream line-buffered widgets split on `\n` anyway.
    let mut merged = String::new();
    let mut cursor = scanner.searched;
    while cursor < bytes.len() {
        let pos = match bytes[cursor..].iter().position(|&b| b == b'\n') {
            Some(pos) => pos,
            None => break,
        };
        let end = cursor + pos;
        merged.push_str(&String::from_utf8_lossy(&bytes[scanner.scan..end]));
        merged.push('\n');
        cursor = end + 1;
        scanner.scan = cursor;
    }
    scanner.searched = bytes.len();
    if flush && scanner.scan < bytes.len() {
        merged.push_str(&String::from_utf8_lossy(&bytes[scanner.scan..]));
        merged.push('\n');
        scanner.scan = bytes.len();
    }
    if !merged.is_empty() {
        ctx.emit_output(id, name, call, &merged).await;
    }
}

/// Read `stdout` and `stderr` concurrently (an epoll-style `select!` loop),
/// appending their raw bytes to `out`/`err` and mirroring completed lines to
/// the live output pane. Returns the first I/O error encountered, otherwise
/// `Ok(())` once both pipes reach EOF. Extracted verbatim from `bash_invoke`
/// so the concurrent-read logic lives in one named place.
async fn read_both_streams(
    id: &str,
    call: &str,
    ctx: &AgentLoopRuntime,
    mut stdout: Option<ChildStdout>,
    mut stderr: Option<ChildStderr>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let mut stdout_done = stdout.is_none();
    let mut stderr_done = stderr.is_none();
    let mut read_error: Option<std::io::Error> = None;
    let mut out_scanner = LineScanner::new();
    let mut err_scanner = LineScanner::new();
    loop {
        if read_error.is_some() || (stdout_done && stderr_done) {
            break;
        }
        tokio::select! {
            r = async {
                stdout.as_mut().unwrap().read(&mut out_buf).await
            }, if !stdout_done => {
                match r {
                    Ok(0) => {
                        stdout_done = true;
                        stdout = None;
                    }
                    Ok(n) => {
                        out.extend_from_slice(&out_buf[..n]);
                        emit_live_lines(id, "Bash", call, ctx, out.as_slice(), &mut out_scanner, false)
                            .await;
                    }
                    Err(e) => {
                        stdout = None;
                        read_error = Some(e);
                    }
                }
            }
            r = async {
                stderr.as_mut().unwrap().read(&mut err_buf).await
            }, if !stderr_done => {
                match r {
                    Ok(0) => {
                        stderr_done = true;
                        stderr = None;
                    }
                    Ok(n) => {
                        err.extend_from_slice(&err_buf[..n]);
                        emit_live_lines(id, "Bash", call, ctx, err.as_slice(), &mut err_scanner, false)
                            .await;
                    }
                    Err(e) => {
                        stderr = None;
                        read_error = Some(e);
                    }
                }
            }
        }
    }
    if let Some(e) = read_error {
        return Err(e);
    }
    emit_live_lines(
        id,
        "Bash",
        call,
        ctx,
        out.as_slice(),
        &mut out_scanner,
        true,
    )
    .await;
    emit_live_lines(
        id,
        "Bash",
        call,
        ctx,
        err.as_slice(),
        &mut err_scanner,
        true,
    )
    .await;
    Ok(())
}

/// Apply `ctx.base_env` / `ctx.extra_env` to `cmd`, clearing the inherited
/// environment when either is configured.
pub(super) fn apply_env_to_command(cmd: &mut Command, ctx: &AgentLoopRuntime) {
    if let Some(base) = &ctx.base_env {
        let mut env = base.clone();
        if let Some(extra) = &ctx.extra_env {
            env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        cmd.env_clear().envs(env);
    } else if let Some(extra) = &ctx.extra_env {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        cmd.env_clear().envs(env);
    }
}

pub(crate) async fn bash_invoke(ctx: &AgentLoopRuntime, args_json: &str, id: &str) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let command = match req_str(&args, "command") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let call = format_tool_call_parts("Bash", &args);
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(BASH_TIMEOUT_SECS)
        .clamp(1, 3600);
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Start `bash` in its own process group. `kill_on_drop` only kills
        // the direct `bash` child; making it a group leader lets the timeout
        // path (and the dropped-tool-future path, via `kill_on_drop` plus
        // the timeout killpg below) stop every descendant too.
        cmd.as_std_mut().process_group(0);
    }
    if let Some(cwd) = &ctx.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(project_root());
    }
    apply_env_to_command(&mut cmd, ctx);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    // Own the process group for the tool call. When the surrounding
    // `join_all` future is dropped on Esc, this guard is dropped too and
    // kills the whole group — not just the direct `bash` child.
    #[cfg(unix)]
    let mut process_group_guard = ProcessGroupGuard::new(child.id());

    // Take the pipes explicitly so the read loop can process stdout and
    // stderr concurrently and emit completed lines to the live pane while
    // keeping the exact same raw stdout-then-stderr result string.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let read_all = read_both_streams(id, &call, ctx, stdout, stderr, &mut out, &mut err);

    let run = async {
        read_all.await?;
        child.wait().await
    };
    match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
        Err(_) => {
            // `process_group_guard` fires as this function returns; the
            // direct child is also covered by `kill_on_drop` when the
            // timed-out future was dropped.
            "[timeout]".into()
        }
        Ok(Err(e)) => {
            #[cfg(unix)]
            process_group_guard.disarm();
            format!("Error: {e}")
        }
        Ok(Ok(status)) => {
            #[cfg(unix)]
            process_group_guard.disarm();
            let mut text = String::from_utf8_lossy(&out).into_owned();
            text.push_str(&String::from_utf8_lossy(&err));
            if status.success() {
                text
            } else {
                format!("[exit {}]\n{text}", status.code().unwrap_or(1))
            }
        }
    }
}
