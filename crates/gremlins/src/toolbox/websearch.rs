use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::clients::format::format_tool_call_parts;
use crate::clients::tools::{parse_args, req_str, AgentLoopRuntime, ToolHandler};

use super::bash::{apply_env_to_command, emit_live_lines, LineScanner, ProcessGroupGuard};

const WEBSEARCH_TIMEOUT_SECS: u64 = 30;

pub(crate) struct WebSearchTool;

#[async_trait]
impl ToolHandler for WebSearchTool {
    async fn invoke(&self, id: &str, ctx: &AgentLoopRuntime, args_json: &str) -> String {
        websearch_invoke(ctx, args_json, id).await
    }
}

fn build_websearch_cmd(
    url: &str,
    timeout_secs: u64,
    max_filesize: u64,
    ctx: &AgentLoopRuntime,
) -> Command {
    let mut cmd = Command::new("curl");
    // `-q` disables `.curlrc` so a local config cannot rewrite the request.
    // `-sS` silences the progress meter but still surfaces errors on stderr.
    // `--proto`/`--proto-redir` restrict both the initial transfer and any
    // `-L` redirects to HTTP(S), blocking `file://` and other schemes.
    cmd.arg("-qsSL")
        .arg("--proto")
        .arg("=http,https")
        .arg("--proto-redir")
        .arg("=http,https")
        .arg("--max-filesize")
        .arg(max_filesize.to_string())
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("--")
        .arg(url);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    if let Some(cwd) = &ctx.cwd {
        cmd.current_dir(cwd);
    }
    apply_env_to_command(&mut cmd, ctx);
    cmd
}

fn format_websearch_output(
    combined: Vec<u8>,
    marker: &str,
    capped: bool,
    status: std::process::ExitStatus,
) -> String {
    let text = String::from_utf8_lossy(&combined).trim().to_string();
    if capped {
        format!("{marker}{text}")
    } else if status.success() {
        text
    } else {
        let code = status.code().unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = status.signal() {
                    return 128 + signal;
                }
            }
            #[allow(unreachable_code)]
            1
        });
        format!("[exit {code}]\n{text}")
    }
}

pub(crate) async fn websearch_invoke(ctx: &AgentLoopRuntime, args_json: &str, id: &str) -> String {
    let cap = crate::config::max_tool_output_bytes();
    websearch_invoke_with_cap(ctx, args_json, cap, id).await
}

pub(crate) async fn websearch_invoke_with_cap(
    ctx: &AgentLoopRuntime,
    args_json: &str,
    cap: u64,
    id: &str,
) -> String {
    let args = match parse_args(args_json) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let url = match req_str(&args, "url") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let call = format_tool_call_parts("WebSearch", &args);
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(WEBSEARCH_TIMEOUT_SECS)
        .clamp(1, 3600);

    // The truncated result must fit inside `cap`. Reserve room for the marker
    // up front, so the outer `invoke` cap sees `len <= cap` and keeps the
    // capped page body instead of replacing it with the generic limit error.
    let marker = if cap > 0 {
        let marker = format!("[truncated at {cap} bytes]\n");
        if marker.len() as u64 <= cap {
            marker
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let data_limit = if cap > 0 {
        cap.checked_sub(marker.len() as u64).map(|n| n as usize)
    } else {
        None
    };

    // Cap the download size so a huge response cannot exhaust memory before
    // `invoke` applies `max_tool_output_bytes`. `--max-filesize` makes curl
    // abort the transfer once the cap is exceeded; we also stop reading at the
    // cap and kill the process group as a belt-and-suspenders measure.
    let max_filesize = if cap > 0 { cap } else { 64 * 1024 * 1024 };
    let mut cmd = build_websearch_cmd(url, timeout_secs, max_filesize, ctx);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    // `curl` is a group leader; its pgid matches its pid on POSIX. Capture
    // the pid *before* taking stdout/stderr because `tokio::process::Child::
    // id()` returns `None` once the handles have been reaped.
    #[cfg(unix)]
    let child_pid = child.id();
    #[cfg(unix)]
    let mut process_group_guard = ProcessGroupGuard::new(child_pid);
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let read_all = async {
        use tokio::io::AsyncReadExt;
        let mut capped = false;
        let mut stdout_done = stdout.is_none();
        let mut stderr_done = stderr.is_none();
        let out_limit = data_limit;
        let err_limit = data_limit;
        let mut out_scanner = LineScanner::new();
        let mut err_scanner = LineScanner::new();
        loop {
            if capped || (stdout_done && stderr_done) {
                break;
            }
            tokio::select! {
                r = async {
                    stdout.as_mut().unwrap().read(&mut out_buf).await
                }, if !stdout_done => {
                    match r {
                        Ok(0) | Err(_) => stdout_done = true,
                        Ok(n) => {
                            let remaining = out_limit.map_or(n, |limit| limit.saturating_sub(out.len()));
                            if remaining == 0 {
                                capped = true;
                                break;
                            }
                            let take = n.min(remaining);
                            out.extend_from_slice(&out_buf[..take]);
                            emit_live_lines(id, "WebSearch", &call, ctx, &out, &mut out_scanner, false)
                                .await;
                            if take < n {
                                capped = true;
                                break;
                            }
                        }
                    }
                }
                r = async {
                    stderr.as_mut().unwrap().read(&mut err_buf).await
                }, if !stderr_done => {
                    match r {
                        Ok(0) | Err(_) => stderr_done = true,
                        Ok(n) => {
                            // curl keeps emitting diagnostics after it trips
                            // `--max-filesize` ("Exceeded the maximum allowed"),
                            // so stderr may fill first and the combined output
                            // can legitimately be empty when stdout never got
                            // a chance to deliver body bytes. Prefer body bytes
                            // when they exist; otherwise fall back to stderr.
                            let body_room = out_limit.map_or(n, |limit| limit.saturating_sub(out.len()));
                            let err_room = err_limit.map_or(n, |limit| limit.saturating_sub(err.len()));
                            if body_room > 0 {
                                let take = n.min(err_room);
                                err.extend_from_slice(&err_buf[..take]);
                                emit_live_lines(id, "WebSearch", &call, ctx, &err, &mut err_scanner, false)
                                    .await;
                            } else if err_room == 0 {
                                stderr_done = true;
                            }
                        }
                    }
                }
            }
        }
        emit_live_lines(id, "WebSearch", &call, ctx, &out, &mut out_scanner, true).await;
        emit_live_lines(id, "WebSearch", &call, ctx, &err, &mut err_scanner, true).await;
        capped
    };
    let capped = match tokio::time::timeout(Duration::from_secs(timeout_secs), read_all).await {
        Err(_) => {
            // `process_group_guard` fires as this function returns.
            return "[timeout]".into();
        }
        Ok(capped) => capped,
    };
    if capped {
        // Stop the transfer now that we have enough bytes.
        #[cfg(unix)]
        process_group_guard.kill();
        let _ = child.kill().await;
    }
    let status = match child.wait().await {
        Ok(s) => s,
        Err(e) => {
            #[cfg(unix)]
            process_group_guard.disarm();
            return format!("Error: {e}");
        }
    };
    #[cfg(unix)]
    process_group_guard.disarm();
    let mut combined = out;
    combined.extend_from_slice(&err);
    if let Some(limit) = data_limit {
        combined.truncate(limit);
    }
    format_websearch_output(combined, &marker, capped, status)
}
