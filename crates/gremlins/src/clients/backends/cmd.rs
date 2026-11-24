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
    async fn pipeline_route_survives_retry_reap_and_changed_current_target() {
        use crate::clients::interactive::InteractiveSessionFactory;

        let run_id = "cmd.pipeline-route.retry";
        let route = CorrId::new()
            .with_run(run_id)
            .with_stage("implement")
            .with_stage_run_id("pipeline-stage")
            .with_attempt(3);
        let (sub, mut events) = gremlins_magic_bus::subscribe_run(run_id);
        let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
        let (_dir, backend) = scripted_backend(
            r#"prompt=$(cat)
case "$prompt" in
  prompt) session=retry-session ;;
  'other prompt') session=other-session ;;
  *) exit 10 ;;
esac
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$session\",\"model\":\"fake\",\"cwd\":\"/tmp\"}"
case "$*" in
  *--resume*)
    [ "$prompt" = prompt ] && [ "$session" = retry-session ] || exit 11
    case "$*" in *'--resume retry-session'*) ;; *) exit 12 ;; esac
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"retry output"}]}}'
    printf '%s\n' '{"type":"result","subtype":"success","result":"done","total_cost_usd":0.25,"num_turns":2}'
    printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"late output"}]}}'
    ;;
  *) printf '%s\n' '{"type":"result","subtype":"error","is_error":true,"api_error_status":503}' ;;
esac
"#,
        );
        let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
        let session = factory.next_session_with_commands(false, None).unwrap();
        let mut params = RunParams::empty();
        params.prompt = "prompt".into();
        params.gremlin_id = Some(run_id.into());
        params.label = "worker".into();
        params.output_corr_id = Some(route.clone());
        params.capture_events = true;
        assert!(matches!(
            backend.run(params, session).await,
            Err(ClientError::ApiServerError { .. })
        ));
        assert!(next_log(&mut events, &route, run_id)
            .await
            .contains("using client"));
        assert!(next_log(&mut events, &route, run_id)
            .await
            .contains("final:"));

        backend.reap_all(run_id);
        let successor = factory.next_session_with_commands(false, None).unwrap();
        assert_ne!(handle.target().stage_run_id(), route.stage_run_id());
        let mut other = RunParams::empty();
        let other_route = route.clone().with_stage_run_id("other-stage-execution");
        other.gremlin_id = Some(run_id.into());
        other.output_corr_id = Some(other_route.clone());
        other.prompt = "other prompt".into();
        other.label = "other worker".into();
        assert!(matches!(
            backend.run(other, successor).await,
            Err(ClientError::ApiServerError { .. })
        ));
        assert_eq!(
            backend.last_route.lock().unwrap().as_ref(),
            Some(&other_route)
        );
        for expected in ["using client", "final:"] {
            assert!(next_log(&mut events, &other_route, run_id)
                .await
                .contains(expected));
        }
        let api: &dyn Backend = &backend;
        let unknown_route = route.clone().with_stage_run_id("unknown-invocation");
        assert!(matches!(
            api.resume_for(&unknown_route).await,
            Err(ClientError::Runtime { message }) if message.contains("no context")
        ));
        let completed = api.resume_for(&route).await.unwrap();
        assert_eq!(completed.text_result.as_deref(), Some("done"));
        assert_eq!(completed.cost_usd, Some(0.25));
        assert_eq!(completed.events.as_ref().unwrap().len(), 4);
        for expected in [
            "using client",
            "retry output",
            "final: subtype=success turns=2 cost=0.25",
            "late output",
        ] {
            let line = next_log(&mut events, &route, run_id).await;
            assert!(
                line.contains(expected),
                "{line:?} did not contain {expected:?}"
            );
            assert!(
                line.starts_with("[worker] "),
                "retry lost A's label: {line}"
            );
        }
        assert!(events.try_recv().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonpipeline_logs_keep_snapshot_execution_in_actual_envelopes() {
        use crate::clients::interactive::InteractiveSessionFactory;

        let run_id = "cmd.nonpipeline-route";
        let (sub, mut events) = gremlins_magic_bus::subscribe_run(run_id);
        let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
        let (_dir, backend) = scripted_backend(
            r#"cat >/dev/null
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
        let completed = backend.run(params, session).await.unwrap();
        assert_eq!(completed.cost_usd, Some(0.5));
        assert!(next_log(&mut events, &route, run_id)
            .await
            .contains("cost=0.5"));
    }

    #[cfg(unix)]
    fn invocation_params(run_id: &str, execution: &str) -> (RunParams, CorrId) {
        let route = CorrId::new()
            .with_run(run_id)
            .with_stage_run_id(execution);
        let mut params = RunParams::empty();
        params.gremlin_id = Some(run_id.into());
        params.output_corr_id = Some(route.clone());
        params.prompt = "original prompt".into();
        (params, route)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invocation_is_registered_before_first_run_await_is_cancelled() {
        use crate::clients::interactive::InteractiveSessionFactory;
        use std::future::Future;

        let run_id = "cmd.cancel-before-init";
        let (_dir, backend) = scripted_backend("cat >/dev/null\nexec sleep 30\n");
        let (params, route) = invocation_params(run_id, "cancelled");
        let (mut factory, _handle) = InteractiveSessionFactory::for_pipeline();
        let session = factory.next_session_with_commands(false, None).unwrap();
        let mut run = Box::pin(backend.run(params, session));
        std::future::poll_fn(|cx| {
            assert!(run.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(run);
        let saved = backend.retry_context(&route);
        backend.reap_all(run_id);
        let saved = saved.expect("cancellation lost invocation context");
        assert_eq!(saved.prompt, "original prompt");
        assert_eq!(saved.last_session_id, None);
        assert!(!saved.completed);
        assert_eq!(backend.last_route.lock().unwrap().as_ref(), Some(&route));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn init_session_is_saved_before_cancellation_and_used_by_resume_for() {
        use crate::clients::interactive::InteractiveSessionFactory;

        let run_id = "cmd.cancel-after-init";
        let (_dir, backend) = scripted_backend(
            r#"prompt=$(cat)
[ "$prompt" = 'original prompt' ] || exit 10
case "$*" in
  *'--resume saved-session'*)
    printf '%s\n' '{"type":"result","subtype":"success","result":"resumed"}' ;;
  *--resume*) exit 11 ;;
  *)
    printf '%s\n' '{"type":"system","subtype":"init","session_id":"saved-session"}'
    exec sleep 30 ;;
esac
"#,
        );
        let (params, route) = invocation_params(run_id, "cancelled");
        let (mut factory, _handle) = InteractiveSessionFactory::for_pipeline();
        let session = factory.next_session_with_commands(false, None).unwrap();
        let observed = {
            let run = backend.run(params, session);
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => panic!("run finished before cancellation: {result:?}"),
                observed = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if backend.retry_context(&route).ok().and_then(|ctx| ctx.last_session_id)
                            == Some("saved-session".into())
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }) => observed,
            }
        };
        backend.reap_all(run_id);
        observed.expect("init session was not persisted while run was pending");
        assert!(!backend.retry_context(&route).unwrap().completed);
        let completed = tokio::time::timeout(Duration::from_secs(5), backend.resume_for(&route))
            .await
            .expect("resume hung")
            .unwrap();
        assert_eq!(completed.text_result.as_deref(), Some("resumed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_retry_init_does_not_replace_established_session() {
        use crate::clients::interactive::InteractiveSessionFactory;

        let (_dir, backend) = scripted_backend(
            r#"cat >/dev/null
case "$*" in
  *'--resume established-session'*) session=different-session ;;
  *--resume*) exit 11 ;;
  *) session=established-session ;;
esac
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$session\"}"
printf '%s\n' '{"type":"result","subtype":"error","is_error":true,"api_error_status":503}'
"#,
        );
        let (params, route) = invocation_params("cmd.failed-retry-init", "failed");
        let (mut factory, _handle) = InteractiveSessionFactory::for_pipeline();
        let session = factory.next_session_with_commands(false, None).unwrap();
        assert!(matches!(
            backend.run(params, session).await,
            Err(ClientError::ApiServerError { .. })
        ));
        for _ in 0..2 {
            assert!(matches!(
                backend.resume_for(&route).await,
                Err(ClientError::ApiServerError { .. })
            ));
            let saved = backend.retry_context(&route).unwrap();
            assert_eq!(saved.last_session_id.as_deref(), Some("established-session"));
            assert!(!saved.completed);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_contexts_are_bounded_but_old_failures_survive_until_completed() {
        use crate::clients::interactive::InteractiveSessionFactory;

        let (_dir, backend) = scripted_backend(
            r#"prompt=$(cat)
case "$prompt:$*" in
  fail:*--resume*) ;;
  fail:*)
    printf '%s\n' '{"type":"system","subtype":"init","session_id":"failed-session"}'
    printf '%s\n' '{"type":"result","subtype":"error","is_error":true,"api_error_status":503}'
    exit 0 ;;
esac
printf '%s\n' '{"type":"result","subtype":"success","result":"done"}'
"#,
        );
        let run_id = "cmd.context-retention";
        let (mut factory, _handle) = InteractiveSessionFactory::for_pipeline();
        let (mut failed, failed_route) = invocation_params(run_id, "failed");
        failed.prompt = "fail".into();
        let session = factory.next_session_with_commands(false, None).unwrap();
        assert!(matches!(
            backend.run(failed, session).await,
            Err(ClientError::ApiServerError { .. })
        ));
        let mut latest_route = None;
        for invocation in 0..8 {
            let (params, route) = invocation_params(run_id, &format!("success-{invocation}"));
            let session = factory.next_session_with_commands(false, None).unwrap();
            backend.run(params, session).await.unwrap();
            let contexts = backend.ctx.lock().unwrap();
            assert_eq!(contexts.len(), 2);
            assert!(!contexts[&failed_route].completed);
            assert!(contexts[&route].completed);
            assert_eq!(contexts.values().filter(|ctx| ctx.completed).count(), 1);
            if let Some(previous) = latest_route.replace(route) {
                assert!(!contexts.contains_key(&previous));
            }
        }
        backend.reap_all(run_id);
        assert!(backend.retry_context(&failed_route).is_ok());
        backend.resume_for(&failed_route).await.unwrap();
        assert!(backend.retry_context(&failed_route).is_err());
        {
            let contexts = backend.ctx.lock().unwrap();
            assert_eq!(contexts.len(), 1);
            assert!(contexts[latest_route.as_ref().unwrap()].completed);
        }

        let mut failed = RunParams::empty();
        failed.prompt = "fail".into();
        failed.output_corr_id = Some(failed_route.clone());
        assert!(backend.run(failed, InteractiveSession::unattached()).await.is_err());
        backend.release_invocation(&failed_route);
        assert!(backend.retry_context(&failed_route).unwrap().completed);
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
    /// Invocation-owned retry state; stages in the same run have distinct routes.
    ctx: Mutex<HashMap<CorrId, CmdContext>>,
    /// Legacy latest-run selection for `resume`; invocation-owned retries
    /// use `resume_for` instead.
    last_route: Mutex<Option<CorrId>>,
}

#[derive(Debug, Clone)]
struct CmdContext {
    prompt: String,
    label: String,
    model: Option<String>,
    raw_path: Option<PathBuf>,
    capture_events: bool,
    on_timeout_prompt: Option<String>,
    cwd: Option<PathBuf>,
    idle_timeout: f64,
    extra_env: Option<HashMap<String, String>>,
    prefix: String,
    last_session_id: Option<String>,
    artifact_dir: Option<PathBuf>,
    gremlin_id: String,
    corr_id: CorrId,
    completed: bool,
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
            ctx: Mutex::new(HashMap::new()),
            last_route: Mutex::new(None),
        })
    }

    /// Capture the retry before yielding: later backend use must not select
    /// another invocation's prompt, session, label, or output route.
    fn retry_context(&self, route: &CorrId) -> Result<CmdContext, ClientError> {
        self.ctx
            .lock()
            .unwrap()
            .get(route)
            .cloned()
            .ok_or_else(|| ClientError::Runtime {
                message: format!("resume(): no context for invocation {route:?}"),
            })
    }

    async fn resume_context(&self, mut ctx: CmdContext) -> Result<CompletedRun, ClientError> {
        let prompt = ctx
            .on_timeout_prompt
            .clone()
            .unwrap_or_else(|| ctx.prompt.clone());
        let session_id = ctx.last_session_id.clone();
        let run = self.attempt(&prompt, session_id.as_deref(), &mut ctx).await?;
        if run.exit_code != 0 {
            return Err(ClientError::Runtime {
                message: format!(
                    "{} (label={}) exited {}",
                    self.command[0], ctx.label, run.exit_code
                ),
            });
        }
        self.complete_context(&ctx);
        Ok(run)
    }

    fn complete_context(&self, context: &CmdContext) {
        let latest = self.last_route.lock().unwrap();
        let mut contexts = self.ctx.lock().unwrap();
        if latest.as_ref() == Some(&context.corr_id) {
            let mut context = context.clone();
            context.completed = true;
            contexts.insert(context.corr_id.clone(), context);
        } else {
            contexts.remove(&context.corr_id);
        }
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
        on_log: Option<&(dyn Fn(String) + Send + Sync)>,
        on_session: &(dyn Fn(&str) + Send + Sync),
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
                    if let Some(id) = &session_id {
                        on_session(id);
                    }
                }
                stream_json::extract_state(&evt, &mut state);
                if let Some(ref mut evts) = events {
                    evts.push(evt.clone());
                }
                stream_json::emit_event(prefix, &evt, on_log);
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

        let argv = self.build_argv(model.as_deref(), session_id, artifact_dir.as_ref());
        let mut child = self
            .spawn(&argv, prompt, cwd.as_ref(), extra_env.as_ref())
            .await?;
        let pid = child.id();
        if let Some(pid) = pid {
            self.track_pid(&gremlin_id, pid);
        }

        let result = if self.stream_json {
            let session_route = corr_id.clone();
            let on_session = |id: &str| {
                if let Some(saved) = self.ctx.lock().unwrap().get_mut(&session_route) {
                    // An established retry session must not be replaced by a failed retry.
                    saved.last_session_id.get_or_insert_with(|| id.to_owned());
                }
            };
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
            let (state, events, timed_out, sid) = Self::read_stream_json(
                &mut child,
                &prefix,
                raw_path.as_ref(),
                capture_events,
                idle_timeout,
                Some(on_log),
                &on_session,
            )
            .await?;

            if let Some(sid) = sid {
                ctx.last_session_id = Some(sid);
            }

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
            prompt: effective_prompt.clone(),
            label: params.label.clone(),
            model: params.model.clone(),
            raw_path: params.raw_path.clone(),
            capture_events: params.capture_events,
            on_timeout_prompt: params.on_timeout_prompt.clone(),
            cwd: params.cwd.clone(),
            idle_timeout,
            extra_env: params.extra_env.clone(),
            prefix: prefix.clone(),
            last_session_id: None,
            artifact_dir: params.artifact_dir.clone(),
            gremlin_id: gremlin_id.clone(),
            corr_id: corr_id.clone(),
            completed: false,
        };
        {
            let mut latest = self.last_route.lock().unwrap();
            let mut contexts = self.ctx.lock().unwrap();
            contexts.retain(|_, context| !context.completed);
            contexts.insert(corr_id.clone(), context.clone());
            *latest = Some(corr_id);
        }
        let result = self.attempt(&effective_prompt, None, &mut context).await;
        if result.as_ref().is_ok_and(|run| run.exit_code == 0) {
            self.complete_context(&context);
        }

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

    fn last_run_params(&self) -> Result<RunParams, ClientError> {
        // CmdBackend resumes saved command sessions rather than replaying params.
        Err(ClientError::Runtime {
            message: "CmdBackend does not expose last_run_params; use resume_for()".into(),
        })
    }

    async fn resume(&self) -> Result<CompletedRun, ClientError> {
        let route = self
            .last_route
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ClientError::Runtime {
                message: "resume() called before run()".into(),
            })?;
        self.resume_for(&route).await
    }

    async fn resume_for(&self, route: &CorrId) -> Result<CompletedRun, ClientError> {
        let context = self.retry_context(route)?;
        self.resume_context(context).await
    }

    fn release_invocation(&self, route: &CorrId) {
        if let Ok(context) = self.retry_context(route) {
            self.complete_context(&context);
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

    fn accepts_interactive_commands(&self) -> bool {
        false
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
