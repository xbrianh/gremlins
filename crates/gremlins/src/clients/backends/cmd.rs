use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use rig_core::driver::DynModel;
use rig_core::operation::Completion;

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::interactive::InteractiveSession;
use crate::clients::params::ResolvedParams;
use crate::clients::protocol::CompletedRun;
use crate::clients::stream_json::{self, StreamState};
use gremlins_magic_bus::{emit_corr_event, CorrId, EventKind};

fn producer_corr_id(
    output_corr_id: Option<&CorrId>,
    run_id: &str,
    stage_run_id: Option<&str>,
) -> CorrId {
    if let Some(corr_id) = output_corr_id {
        return corr_id.clone();
    }
    let mut corr_id = CorrId::new();
    if !run_id.is_empty() {
        corr_id = corr_id.with_run(run_id);
    }
    if let Some(stage_run_id) = stage_run_id {
        corr_id = corr_id.with_stage_run_id(stage_run_id);
    }
    corr_id
}

#[cfg(test)]
mod attribution_tests {
    use super::*;

    #[test]
    fn producer_identity_keeps_run_and_execution_separate() {
        let corr = producer_corr_id(None, "parent:child", Some("opaque:execution"));
        assert_eq!(corr.run(), Some("parent:child"));
        assert_eq!(corr.stage_run_id(), Some("opaque:execution"));
        let payload = EventKind::LogLine {
            id: corr.run().unwrap().to_owned(),
            line: "hello".into(),
        };
        let EventKind::LogLine { id, .. } = payload else {
            unreachable!();
        };
        assert_eq!(id, "parent:child");
    }

    #[test]
    fn explicit_route_is_not_reconstructed_from_current_identity() {
        let route = CorrId::new()
            .with_run("pipeline")
            .with_stage("implement")
            .with_stage_run_id("stage-execution")
            .with_attempt(2);
        assert_eq!(
            producer_corr_id(Some(&route), "other-run", Some("current-target")),
            route
        );
    }

    #[cfg(unix)]
    fn scripted_backend(script: &str) -> (tempfile::TempDir, CmdBackend) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.sh");
        std::fs::write(&path, script).unwrap();
        let backend = CmdBackend::new(&format!(
            "/bin/sh '{}' --output-format stream-json",
            path.display()
        ))
        .unwrap();
        (dir, backend)
    }

    #[cfg(unix)]
    async fn next_log(
        events: &mut gremlins_magic_bus::ReceiverHalf<EventKind>,
        route: &CorrId,
        run_id: &str,
    ) -> String {
        let event = match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("cmd log was not emitted")
        {
            gremlins_magic_bus::Recv::Event(event) => event,
            other => panic!("expected event, got {other:?}"),
        };
        assert_eq!(event.corr_id(), route);
        match event.kind() {
            EventKind::LogLine { id, line } => {
                assert_eq!(id, run_id);
                line.clone()
            }
            other => panic!("expected cmd log, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonpipeline_logs_keep_snapshot_execution_in_actual_envelopes() {
        use crate::clients::interactive::InteractiveSessionFactory;

        let run_id = "cmd.nonpipeline-route";
        let (sub, mut events) = gremlins_magic_bus::subscribe_run(run_id);
        let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
        let (dir, backend) = scripted_backend(
            r#"cat >/dev/null
printf '%s\n' '{"type":"system","subtype":"init","session_id":"unused-session","model":"fake","cwd":"/tmp"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"done","cost_usd":0.5}'
"#,
        );
        let (mut factory, _handle) = InteractiveSessionFactory::for_chat();
        let session = factory.next_session_with_commands(false, None).unwrap();
        let route = CorrId::new()
            .with_run(run_id)
            .with_stage_run_id(session.snapshot().stage_run_id().unwrap());
        let mut params = RunParams::empty();
        params.gremlin_id = Some(run_id.into());
        params.capture_events = true;
        let raw_path = dir.path().join("raw.jsonl");
        params.raw_path = Some(raw_path.clone());
        let completed = backend.run(params, session).await.unwrap();
        assert_eq!(completed.cost_usd, Some(0.5));
        let captured = completed.events.as_ref().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0]["subtype"], "init");
        assert_eq!(captured[0]["session_id"], "unused-session");
        let raw = std::fs::read_to_string(raw_path).unwrap();
        let raw_events: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(&raw_events, captured);
        assert!(next_log(&mut events, &route, run_id)
            .await
            .contains("using client"));
        assert!(next_log(&mut events, &route, run_id)
            .await
            .contains("cost=0.5"));
    }

    #[test]
    fn missing_identity_does_not_invent_an_execution() {
        let corr = producer_corr_id(None, "run", None);
        assert_eq!(corr.run(), Some("run"));
        assert_eq!(corr.stage_run_id(), None);
        assert_eq!(producer_corr_id(None, "", None), CorrId::new());
        let corr = producer_corr_id(None, "", Some("execution"));
        assert_eq!(corr.run(), None);
        assert_eq!(corr.stage_run_id(), Some("execution"));
    }
}

fn footer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\n*⏺ Cost:.*$").expect("invalid footer regex"))
}

pub struct CmdBackend {
    command: Vec<String>,
    stream_json: bool,
    footer_re: Option<Regex>,
    pids: Mutex<HashMap<String, Vec<u32>>>,
}

#[derive(Debug, Clone)]
struct CmdContext {
    model: Option<String>,
    raw_path: Option<PathBuf>,
    capture_events: bool,
    cwd: Option<PathBuf>,
    idle_timeout: f64,
    extra_env: Option<HashMap<String, String>>,
    prefix: String,
    artifact_dir: Option<PathBuf>,
    gremlin_id: String,
    corr_id: CorrId,
}

impl CmdBackend {
    /// Registry-compatible builder. `native_block` and `extra_params` are
    /// ignored — a command backend's model *is* the shell command.
    pub fn build(
        model: &str,
        _native_block: &HashMap<String, Vec<String>>,
        _extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        Ok(Arc::new(Self::new(model)?))
    }

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

    fn build_argv(&self, model: Option<&str>, artifact_dir: Option<&PathBuf>) -> Vec<String> {
        let mut argv = self.command.clone();
        if let Some(m) = model {
            if !argv.iter().any(|a| a == "--model") {
                argv.push("--model".into());
                argv.push(m.into());
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
        on_log: Option<&(dyn Fn(String) + Send + Sync)>,
    ) -> Result<(StreamState, Option<Vec<serde_json::Value>>, bool), ClientError> {
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
                stream_json::extract_state(&evt, &mut state);
                if let Some(ref mut evts) = events {
                    evts.push(evt.clone());
                }
                stream_json::emit_event(prefix, &evt, on_log);
            }
        }

        Ok((state, events, timed_out))
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
        ctx: &mut CmdContext,
    ) -> Result<CompletedRun, ClientError> {
        let gremlin_id = ctx.gremlin_id.clone();
        let (
            model,
            cwd,
            extra_env,
            prefix,
            raw_path,
            capture_events,
            idle_timeout,
            artifact_dir,
            corr_id,
        ) = {
            (
                ctx.model.clone(),
                ctx.cwd.clone(),
                ctx.extra_env.clone(),
                ctx.prefix.clone(),
                ctx.raw_path.clone(),
                ctx.capture_events,
                ctx.idle_timeout,
                ctx.artifact_dir.clone(),
                ctx.corr_id.clone(),
            )
        };

        let argv = self.build_argv(model.as_deref(), artifact_dir.as_ref());
        let mut child = self
            .spawn(&argv, prompt, cwd.as_ref(), extra_env.as_ref())
            .await?;
        let pid = child.id();
        if let Some(pid) = pid {
            self.track_pid(&gremlin_id, pid);
        }

        let result = if self.stream_json {
            let gid = gremlin_id.to_string();
            let on_log: &(dyn Fn(String) + Send + Sync) = &move |msg: String| {
                emit_corr_event(
                    &corr_id,
                    EventKind::LogLine {
                        id: gid.clone(),
                        line: msg,
                    },
                );
            };
            let (state, events, timed_out) = Self::read_stream_json(
                &mut child,
                &prefix,
                raw_path.as_ref(),
                capture_events,
                idle_timeout,
                Some(on_log),
            )
            .await?;

            let status = child.wait().await.map_err(|e| ClientError::Runtime {
                message: format!("wait error: {e}"),
            })?;

            if let Some(pid) = pid {
                self.untrack_pid(&gremlin_id, pid);
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
                summary: None,
                events,
                cost_usd: state.cost_usd,
                token_usage: None,
                conversation: None,
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
                self.untrack_pid(&gremlin_id, pid);
            }

            if timed_out {
                return Err(ClientError::Timeout {
                    message: "stream idle timeout".into(),
                });
            }

            CompletedRun {
                exit_code: status.code().unwrap_or(1),
                text_result: Some(text),
                summary: None,
                events: None,
                cost_usd: None,
                token_usage: None,
                conversation: None,
            }
        };

        Ok(result)
    }
}

#[async_trait]
impl Backend for CmdBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        // Concatenation of harness system prompt and definition user content —
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
        let snapshot = interactive.snapshot();
        let corr_id = producer_corr_id(
            params.output_corr_id.as_ref(),
            &gremlin_id,
            snapshot.stage_run_id(),
        );
        let mut context = CmdContext {
            model: params.model.clone(),
            raw_path: params.raw_path.clone(),
            capture_events: params.capture_events,
            cwd: params.cwd.clone(),
            idle_timeout,
            extra_env: params.extra_env.clone(),
            prefix: prefix.clone(),
            artifact_dir: params.artifact_dir.clone(),
            gremlin_id: gremlin_id.clone(),
            corr_id: corr_id.clone(),
        };
        let result = self.attempt(&effective_prompt, &mut context).await;

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
            Err(e) => Err(e),
        }
    }

    fn accepts_commands(&self) -> bool {
        false
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

    /// Returns the empty string — the [`CmdBackend`] has no single model;
    /// the shell command itself varies per run. See also
    /// [`make_model`](Backend::make_model), which returns `None` for every
    /// spec, ensuring local compaction never reaches this backend.
    fn model(&self) -> &str {
        ""
    }

    fn name(&self) -> &'static str {
        "cmd"
    }

    fn build_model(&self, _spec: &str) -> Option<DynModel<Completion>> {
        None
    }

    fn total_cost_usd(&self) -> Option<f64> {
        None
    }

    fn resolve_params(_raw: &indexmap::IndexMap<String, String>) -> Result<ResolvedParams, String> {
        // A command backend never receives extra params — its model *is* the
        // shell command, so the resolver returns the empty default.
        Ok(ResolvedParams::default())
    }
}
