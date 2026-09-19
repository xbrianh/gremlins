use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::backend::{Backend, ClientError, RunParams};
use super::protocol::CompletedRun;
use super::retry::{validate_max_retries, with_retry, STREAM_IDLE_BACKOFF};
use super::stream;
use super::stream_json::{self, StreamState};

fn footer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\n*⏺ Cost:.*$").expect("invalid footer regex"))
}

pub struct CmdBackend {
    command: Vec<String>,
    stream_json: bool,
    footer_re: Option<Regex>,
    pids: Mutex<HashMap<String, Vec<u32>>>,
    /// Per-gremlin retry context, keyed by gremlin_id so parallel children
    /// don't overwrite each other's prompt/session when a timeout occurs.
    ctx: Mutex<HashMap<String, CmdContext>>,
    /// The gremlin_id most recently passed to `run`, used by `resume`
    /// to look up the right context.
    last_gremlin_id: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
struct CmdContext {
    prompt: String,
    label: String,
    model: Option<String>,
    raw_path: Option<PathBuf>,
    capture_events: bool,
    on_timeout_prompt: Option<String>,
    max_retries: usize,
    cwd: Option<PathBuf>,
    idle_timeout: f64,
    extra_env: Option<HashMap<String, String>>,
    prefix: String,
    last_session_id: Option<String>,
    artifact_dir: Option<PathBuf>,
    gremlin_id: String,
}

impl CmdBackend {
    pub fn new(command: &str) -> Result<Self, String> {
        let args =
            shlex::split(command).ok_or_else(|| format!("failed to parse command: {command}"))?;
        if args.is_empty() {
            return Err("empty command".into());
        }

        let stream_json = args
            .windows(2)
            .any(|pair| pair[0] == "--output-format" && pair[1] == "stream-json")
            || args.iter().any(|a| a == "--output-format=stream-json");

        let footer_re = if args.first().is_some_and(|a| a.contains("copilot")) {
            Some(footer_re().clone())
        } else {
            None
        };

        Ok(CmdBackend {
            command: args,
            stream_json,
            footer_re,
            pids: Mutex::new(HashMap::new()),
            ctx: Mutex::new(HashMap::new()),
            last_gremlin_id: Mutex::new(None),
        })
    }

    fn track_pid(&self, gremlin_id: &str, pid: u32) {
        if let Ok(mut pids) = self.pids.lock() {
            pids.entry(gremlin_id.to_string()).or_default().push(pid);
        }
    }

    fn untrack_pid(&self, gremlin_id: &str, pid: u32) {
        if let Ok(mut pids) = self.pids.lock() {
            if let Some(vec) = pids.get_mut(gremlin_id) {
                vec.retain(|p| *p != pid);
                if vec.is_empty() {
                    pids.remove(gremlin_id);
                }
            }
        }
    }

    fn build_argv(
        &self,
        model: Option<&str>,
        session_id: Option<&str>,
        artifact_dir: Option<&PathBuf>,
    ) -> Vec<String> {
        let mut argv = self.command.clone();
        if let Some(m) = model {
            if !argv.iter().any(|a| a == "--model") {
                argv.push("--model".into());
                argv.push(m.into());
            }
        }
        if let Some(sid) = session_id {
            if !argv.iter().any(|a| a == "--resume") {
                argv.push("--resume".into());
                argv.push(sid.into());
            }
        }
        if let Some(dir) = artifact_dir {
            argv.push("--add-dir".into());
            argv.push(dir.to_string_lossy().into_owned());
        }
        argv
    }

    async fn spawn(
        &self,
        argv: &[String],
        prompt: &str,
        cwd: Option<&PathBuf>,
        extra_env: Option<&HashMap<String, String>>,
    ) -> Result<Child, ClientError> {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);

        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
        }

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut env_vars: HashMap<String, String> = HashMap::new();
        env_vars.insert("GREMLIN_SKIP_SUMMARY".into(), "1".into());
        if let Some(extra) = extra_env {
            env_vars.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| ClientError::Runtime {
            message: format!("failed to spawn {}: {e}", argv[0]),
        })?;

        // Write prompt to stdin, then close it
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|e| ClientError::Runtime {
                    message: format!("failed to write prompt to stdin: {e}"),
                })?;
        }

        Ok(child)
    }

    async fn read_stream_json(
        child: &mut Child,
        prefix: &str,
        raw_path: Option<&PathBuf>,
        capture_events: bool,
        idle_timeout: f64,
    ) -> Result<
        (
            StreamState,
            Option<Vec<serde_json::Value>>,
            bool,
            Option<String>,
        ),
        ClientError,
    > {
        let stdout = child.stdout.take().ok_or_else(|| ClientError::Runtime {
            message: "no stdout".into(),
        })?;

        let mut state = StreamState::default();
        let mut events: Option<Vec<serde_json::Value>> = if capture_events {
            Some(Vec::new())
        } else {
            None
        };
        let mut timed_out = false;
        let mut session_id: Option<String> = None;
        let mut raw = raw_path
            .map(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
            })
            .transpose()
            .map_err(|e| ClientError::Runtime {
                message: format!("failed to open raw_path: {e}"),
            })?;

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        loop {
            let line: String = match tokio::time::timeout(
                Duration::from_secs_f64(idle_timeout),
                lines.next_line(),
            )
            .await
            {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    return Err(ClientError::Runtime {
                        message: format!("read error: {e}"),
                    });
                }
                Err(_) => {
                    timed_out = true;
                    break;
                }
            };

            let line_bytes = line.as_bytes();
            if let Some(ref mut f) = raw {
                let _ = f.write_all(line_bytes);
                let _ = f.write_all(b"\n");
                let _ = f.flush();
            }

            if line.contains("Stream idle timeout") {
                let evt = stream_json::decode_line(line_bytes);
                if evt.is_none() {
                    timed_out = true;
                }
            }

            if let Some(evt) = stream_json::decode_line(line_bytes) {
                if evt.get("type").and_then(|v| v.as_str()) == Some("system")
                    && evt.get("subtype").and_then(|v| v.as_str()) == Some("init")
                {
                    session_id = evt
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                stream_json::extract_state(&evt, &mut state);
                if let Some(ref mut evts) = events {
                    evts.push(evt.clone());
                }
                stream_json::emit_event(prefix, &evt);
            }
        }

        Ok((state, events, timed_out, session_id))
    }

    async fn read_plain(
        child: &mut Child,
        raw_path: Option<&PathBuf>,
        footer_re: Option<&Regex>,
        idle_timeout: f64,
    ) -> Result<(String, bool), ClientError> {
        let stdout = child.stdout.take().ok_or_else(|| ClientError::Runtime {
            message: "no stdout".into(),
        })?;

        let mut output = Vec::new();
        let mut raw = raw_path
            .map(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
            })
            .transpose()
            .map_err(|e| ClientError::Runtime {
                message: format!("failed to open raw_path: {e}"),
            })?;

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut timed_out = false;

        loop {
            let line: String = match tokio::time::timeout(
                Duration::from_secs_f64(idle_timeout),
                lines.next_line(),
            )
            .await
            {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => {
                    return Err(ClientError::Runtime {
                        message: format!("read error: {e}"),
                    });
                }
                Err(_) => {
                    timed_out = true;
                    break;
                }
            };

            let line_bytes = line.as_bytes();
            if let Some(ref mut f) = raw {
                let _ = f.write_all(line_bytes);
                let _ = f.write_all(b"\n");
                let _ = f.flush();
            }

            output.extend_from_slice(line_bytes);
            output.push(b'\n');
        }

        let text = String::from_utf8_lossy(&output).into_owned();
        let text = if let Some(re) = footer_re {
            re.replace(&text, "").trim_end().to_string()
        } else {
            text.trim_end().to_string()
        };

        Ok((text, timed_out))
    }

    async fn attempt(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        gremlin_id: &str,
    ) -> Result<CompletedRun, ClientError> {
        let (model, cwd, extra_env, prefix, raw_path, capture_events, idle_timeout, artifact_dir) = {
            let ctx_guard = self.ctx.lock().unwrap();
            let ctx = ctx_guard.get(gremlin_id).ok_or_else(|| ClientError::Runtime {
                message: format!("attempt() called before run() for gremlin {gremlin_id}"),
            })?;
            (
                ctx.model.clone(),
                ctx.cwd.clone(),
                ctx.extra_env.clone(),
                ctx.prefix.clone(),
                ctx.raw_path.clone(),
                ctx.capture_events,
                ctx.idle_timeout,
                ctx.artifact_dir.clone(),
            )
        };

        let argv = self.build_argv(model.as_deref(), session_id, artifact_dir.as_ref());
        let mut child = self
            .spawn(&argv, prompt, cwd.as_ref(), extra_env.as_ref())
            .await?;
        let pid = child.id();
        if let Some(pid) = pid {
            self.track_pid(gremlin_id, pid);
        }

        let result = if self.stream_json {
            let (state, events, timed_out, sid) = Self::read_stream_json(
                &mut child,
                &prefix,
                raw_path.as_ref(),
                capture_events,
                idle_timeout,
            )
            .await?;

            if let Some(ref sid) = sid {
                if let Ok(mut ctx) = self.ctx.lock() {
                    if let Some(c) = ctx.get_mut(gremlin_id) {
                        c.last_session_id = Some(sid.clone());
                    }
                }
            }

            let status = child.wait().await.map_err(|e| ClientError::Runtime {
                message: format!("wait error: {e}"),
            })?;

            if let Some(pid) = pid {
                self.untrack_pid(gremlin_id, pid);
            }

            if timed_out {
                return Err(ClientError::Timeout {
                    message: "stream idle timeout".into(),
                });
            }

            if state.is_error {
                if let Some(status_code) = state.api_error_status {
                    if (500..=599).contains(&status_code) {
                        return Err(ClientError::ApiServerError {
                            message: format!("api server error {status_code}"),
                        });
                    }
                }
            }

            CompletedRun {
                exit_code: status.code().unwrap_or(1),
                text_result: state.result_text,
                events,
                cost_usd: state.cost_usd,
                token_usage: None,
            }
        } else {
            let (text, timed_out) = Self::read_plain(
                &mut child,
                raw_path.as_ref(),
                self.footer_re.as_ref(),
                idle_timeout,
            )
            .await?;

            let status = child.wait().await.map_err(|e| ClientError::Runtime {
                message: format!("wait error: {e}"),
            })?;

            if let Some(pid) = pid {
                self.untrack_pid(gremlin_id, pid);
            }

            if timed_out {
                return Err(ClientError::Timeout {
                    message: "stream idle timeout".into(),
                });
            }

            CompletedRun {
                exit_code: status.code().unwrap_or(1),
                text_result: Some(text),
                events: None,
                cost_usd: None,
                token_usage: None,
            }
        };

        Ok(result)
    }
}

#[async_trait]
impl Backend for CmdBackend {
    async fn run(&self, params: RunParams) -> Result<CompletedRun, ClientError> {
        validate_max_retries(params.max_retries)
            .map_err(|m| ClientError::Runtime { message: m })?;

        // Concatenation of harness system prompt and pipeline user content —
        // this is the single string bound for the child process's stdin.
        let effective_prompt = match &params.system_prompt {
            Some(sys) => format!("{}\n\n{}", sys, params.prompt),
            None => params.prompt.clone(),
        };

        let idle_timeout = params
            .idle_timeout
            .unwrap_or_else(crate::config::stream_idle_timeout);
        let prefix = if params.label.is_empty() {
            String::new()
        } else {
            format!("[{}] ", params.label)
        };

        let gremlin_id = params.gremlin_id.clone().unwrap_or_default();
        {
            let mut ctx = self.ctx.lock().unwrap();
            ctx.insert(gremlin_id.clone(), CmdContext {
                prompt: effective_prompt.clone(),
                label: params.label.clone(),
                model: params.model.clone(),
                raw_path: params.raw_path.clone(),
                capture_events: params.capture_events,
                on_timeout_prompt: params.on_timeout_prompt.clone(),
                max_retries: params.max_retries,
                cwd: params.cwd.clone(),
                idle_timeout,
                extra_env: params.extra_env.clone(),
                prefix: prefix.clone(),
                last_session_id: None,
                artifact_dir: params.artifact_dir.clone(),
                gremlin_id: gremlin_id.clone(),
            });
            *self.last_gremlin_id.lock().unwrap() = Some(gremlin_id.clone());
        }

        let result = self.attempt(&effective_prompt, None, &gremlin_id).await;

        match result {
            Ok(r) => {
                if r.exit_code != 0 {
                    return Err(ClientError::Runtime {
                        message: format!(
                            "{} (model={:?}, label={}) exited {}",
                            self.command[0], params.model, params.label, r.exit_code
                        ),
                    });
                }
                Ok(r)
            }
            Err(ClientError::Timeout { .. }) | Err(ClientError::ApiServerError { .. }) => {
                self.resume().await
            }
            Err(e) => Err(e),
        }
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        let (prompt, on_timeout_prompt, max_retries, prefix, last_session_id, gremlin_id) = {
            let gid = self.last_gremlin_id.lock().unwrap();
            let gid = gid.as_ref().ok_or_else(|| ClientError::Runtime {
                message: "resume() called before run()".into(),
            })?;
            let ctx = self.ctx.lock().unwrap();
            let ctx = ctx.get(gid).ok_or_else(|| ClientError::Runtime {
                message: format!("resume(): no context for gremlin {gid}"),
            })?;
            (
                ctx.prompt.clone(),
                ctx.on_timeout_prompt.clone(),
                ctx.max_retries,
                ctx.prefix.clone(),
                ctx.last_session_id.clone(),
                ctx.gremlin_id.clone(),
            )
        };

        let backoff = &STREAM_IDLE_BACKOFF[..max_retries.saturating_sub(1)];

        let active_prompt = prompt.clone();

        let result = with_retry(
            backoff,
            |e: &ClientError| {
                matches!(
                    e,
                    ClientError::Timeout { .. } | ClientError::ApiServerError { .. }
                )
            },
            |attempt, e, wait| {
                let cause = match e {
                    ClientError::Timeout { .. } => "stream idle timeout",
                    ClientError::ApiServerError { .. } => "api server error",
                    _ => "error",
                };
                eprintln!(
                    "{} {}{}, resuming in {}s ({}/{})...",
                    stream::ts_internal(),
                    prefix,
                    cause,
                    wait,
                    attempt + 1,
                    max_retries
                );
            },
            || {
                let p = if on_timeout_prompt.is_some() {
                    on_timeout_prompt.clone().unwrap()
                } else {
                    active_prompt.clone()
                };
                let sid = last_session_id.clone();
                let gid = gremlin_id.clone();
                async move { self.attempt(&p, sid.as_deref(), &gid).await }
            },
        )
        .await;

        match result {
            Ok(r) => {
                if r.exit_code != 0 {
                    let label = {
                        let gid = self.last_gremlin_id.lock().unwrap();
                        let gid_opt = gid.as_ref();
                        let ctx = self.ctx.lock().unwrap();
                        gid_opt
                            .and_then(|gid| ctx.get(gid.as_str()))
                            .map_or("?".to_string(), |c| c.label.clone())
                    };
                    return Err(ClientError::Runtime {
                        message: format!(
                            "{} (label={}) exited {}",
                            self.command[0], label, r.exit_code
                        ),
                    });
                }
                Ok(r)
            }
            Err(e) => Err(e),
        }
    }

    fn reap_all(&self, gremlin_id: &str) {
        let pids: Vec<u32> = {
            let mut pids = self.pids.lock().unwrap();
            pids.remove(gremlin_id).unwrap_or_default()
        };
        #[cfg(unix)]
        for &pid in &pids {
            unsafe {
                libc::killpg(pid as i32, libc::SIGTERM);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        #[cfg(unix)]
        for &pid in &pids {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }
}
