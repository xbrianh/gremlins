//! Done assessor for DSL agent stages: runs the stage's `done_check` shell
//! command on every proposed `Done` and translates its exit status.
//!
//! * exit 0 — accept
//! * exit 1 — retry; bounded output goes back to the model
//! * anything else (including signals, spawn failure, timeout) — error
//!
//! The required-output predicate is the real `gremlins artifact <id> --exists`
//! CLI, run under a pending-output scope whose token reaches only that command
//! and is revoked when the invocation ends, including on cancellation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::executor::supervisor::run_registry::RunRegistry;
use crate::stage_spec::done_check::{DoneCheck, DoneContext, DoneVerdict};

/// Option key holding the shell command.
/// Marker option set by the DSL parser: this stage's required outputs are
/// checked through the real `gremlins artifact --exists` CLI.
pub(crate) const DSL_STAGE_KEY: &str = "dsl_stage";
pub(crate) const DONE_CHECK_KEY: &str = "done_check";
const SCOPE_ENV: &str = "GREMLINS_ARTIFACT_CHECK_SCOPE";
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
const FALLBACK_RETRY: &str = "The done_check command rejected this completion; \
fix the problem and call Done again.";

/// Runtime data for the generated `gremlins artifact <id> --exists` predicate.
pub(crate) struct ArtifactParams {
    pub run_id: String,
    pub registry: Arc<RunRegistry>,
    pub env: HashMap<String, String>,
    pub checkout_root: PathBuf,
    /// (uri, resolved destination) of every declared output.
    pub scope_outputs: Vec<(String, PathBuf)>,
    /// URIs of the required outputs, passed as literal arguments.
    pub required_uris: Vec<String>,
}

/// Runtime data for the author's `done_check` command.
pub(crate) struct DslDoneParams {
    /// Already interpolated with the stage substitutions.
    pub command: String,
    pub env: HashMap<String, String>,
}

/// The required-output predicate backed by the real CLI. The pending-output
/// scope lives only for one invocation and its token reaches only that command.
pub(crate) fn dsl_artifact_check(params: ArtifactParams) -> DoneCheck {
    let params = Arc::new(params);
    DoneCheck::new(move |ctx| {
        let params = params.clone();
        async move { run_artifact_check(&params, ctx).await }
    })
    .reaping()
}

/// The author's command, run only after the artifact predicate passed.
pub(crate) fn dsl_done_check(params: DslDoneParams) -> DoneCheck {
    let params = Arc::new(params);
    DoneCheck::new(move |ctx| {
        let params = params.clone();
        async move { run_custom_check(&params, ctx).await }
    })
    .reaping()
}

async fn run_artifact_check(params: &ArtifactParams, ctx: DoneContext) -> DoneVerdict {
    let guard = match params.registry.open_pending_scope(
        &params.run_id,
        params.checkout_root.clone(),
        params.scope_outputs.clone(),
    ) {
        Ok(guard) => guard,
        Err(e) => return DoneVerdict::Error(format!("pending-output scope unavailable: {e}")),
    };
    let mut env = params.env.clone();
    env.insert(SCOPE_ENV.to_string(), guard.token().to_string());
    put_exe_dir_on_path(&mut env);

    let mut cmd = tokio::process::Command::new("gremlins");
    cmd.args(["artifact", &params.run_id, "--exists"])
        .args(&params.required_uris);
    let verdict = run_process(cmd, &ctx.cwd, &env, &ctx, "artifact check", CHECK_TIMEOUT).await;
    drop(guard);
    verdict
}

async fn run_custom_check(params: &DslDoneParams, ctx: DoneContext) -> DoneVerdict {
    let mut env = params.env.clone();
    env.remove(SCOPE_ENV);
    env.insert("GREMLINS_DONE_SUMMARY".into(), ctx.summary.clone());
    for output in &ctx.outputs {
        let key: String = output
            .key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        env.insert(format!("GREMLINS_OUTPUT_{key}"), output.uri.clone());
    }
    put_exe_dir_on_path(&mut env);
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&params.command);
    run_process(cmd, &ctx.cwd, &env, &ctx, "done_check", CHECK_TIMEOUT).await
}

fn put_exe_dir_on_path(env: &mut HashMap<String, String>) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if exe.file_stem().and_then(|s| s.to_str()) != Some("gremlins") {
        return;
    }
    let Some(dir) = exe.parent() else { return };
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env.get("PATH") {
        paths.extend(std::env::split_paths(existing));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        env.insert("PATH".into(), joined.to_string_lossy().into_owned());
    }
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_DIAGNOSTIC_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    kept
}

fn kill_group(pid: u32) {
    // SAFETY: plain signal delivery to the child's own process group.
    unsafe {
        libc::killpg(pid as i32, libc::SIGKILL);
    }
}

/// Kills the child's whole process group when dropped, so a future dropped
/// mid-flight (cancellation) cannot leave descendants running.
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

async fn run_process(
    mut cmd: tokio::process::Command,
    cwd: &Path,
    env: &HashMap<String, String>,
    ctx: &DoneContext,
    label: &str,
    timeout: Duration,
) -> DoneVerdict {
    cmd.current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return DoneVerdict::Error(format!("{label} failed to start: {e}")),
    };
    let mut group = Some(GroupGuard(child.id()));
    let stdout = read_bounded(child.stdout.take().expect("piped"));
    let stderr = read_bounded(child.stderr.take().expect("piped"));
    let collect = async {
        let wait = async {
            let status = child.wait().await;
            // Close pipes held by stragglers in the original process group.
            drop(group.take());
            status
        };
        tokio::join!(wait, stdout, stderr)
    };
    // A descendant outside the group can retain either pipe even after wait().
    // Keep the readers in this select so interruption drops them instead of
    // leaving detached tasks waiting for EOF.
    let result = tokio::select! {
        result = collect => Ok(result),
        _ = ctx.cancel.cancelled() => Err(format!("{label} cancelled")),
        _ = tokio::time::sleep(timeout) => Err(format!("{label} timed out after {}s", timeout.as_secs())),
    };
    let (status, out, err) = match result {
        Ok(result) => result,
        Err(message) => {
            drop(group);
            let _ = child.kill().await;
            let _ = child.wait().await;
            return DoneVerdict::Error(message);
        }
    };
    let status = match status {
        Ok(s) => s,
        Err(e) => return DoneVerdict::Error(format!("{label} wait failed: {e}")),
    };
    let diagnostic = {
        let mut text = String::from_utf8_lossy(&err).trim().to_string();
        let out = String::from_utf8_lossy(&out).trim().to_string();
        if !out.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&out);
        }
        text
    };
    match status.code() {
        Some(0) => DoneVerdict::Accept,
        Some(1) if diagnostic.is_empty() => DoneVerdict::Retry(FALLBACK_RETRY.into()),
        Some(1) => DoneVerdict::Retry(diagnostic),
        Some(code) => DoneVerdict::Error(format!("{label} exited {code}: {diagnostic}")),
        None => DoneVerdict::Error(format!("{label} was killed by a signal: {diagnostic}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::CancelToken;

    fn ctx(cancel: Arc<CancelToken>) -> DoneContext {
        DoneContext {
            summary: "all done".into(),
            cwd: std::env::temp_dir(),
            outputs: vec![],
            cancel,
        }
    }

    async fn verdict(command: &str) -> DoneVerdict {
        let check = dsl_done_check(DslDoneParams {
            command: command.into(),
            env: HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        });
        check.call(ctx(CancelToken::new())).await
    }

    #[tokio::test]
    async fn exit_codes_translate() {
        assert_eq!(verdict("exit 0").await, DoneVerdict::Accept);
        assert_eq!(
            verdict("echo nope >&2; exit 1").await,
            DoneVerdict::Retry("nope".into())
        );
        assert_eq!(
            verdict("exit 1").await,
            DoneVerdict::Retry(FALLBACK_RETRY.into())
        );
        assert!(matches!(verdict("exit 2").await, DoneVerdict::Error(_)));
        assert_eq!(
            verdict("test \"$GREMLINS_DONE_SUMMARY\" = 'all done'").await,
            DoneVerdict::Accept
        );
    }

    #[tokio::test]
    async fn substituted_value_arrives_as_one_literal_argument() {
        let dir = tempfile::tempdir().unwrap();
        let value = "a b; touch pwned $(id) 'q'";
        let check = dsl_done_check(DslDoneParams {
            command: "set -- \"$GREMLINS_V\"; printf '%s\\n%s' \"$#\" \"$1\" > out; \
                      test -z \"$GREMLINS_ARTIFACT_CHECK_SCOPE\""
                .into(),
            env: HashMap::from([
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("GREMLINS_V".to_string(), value.to_string()),
                (SCOPE_ENV.to_string(), "secret".to_string()),
            ]),
        });
        let mut context = ctx(CancelToken::new());
        context.cwd = dir.path().to_path_buf();
        assert_eq!(check.call(context).await, DoneVerdict::Accept);
        let recorded = std::fs::read_to_string(dir.path().join("out")).unwrap();
        assert_eq!(recorded, format!("1\n{value}"));
        assert!(!dir.path().join("pwned").exists());
    }

    #[tokio::test]
    async fn launch_failure_is_an_error_not_a_retry() {
        let check = dsl_done_check(DslDoneParams {
            command: "exit 0".into(),
            env: HashMap::new(),
        });
        let mut context = ctx(CancelToken::new());
        context.cwd = std::path::PathBuf::from("/nonexistent/gremlins-cwd");
        match check.call(context).await {
            DoneVerdict::Error(message) => assert!(message.contains("failed to start")),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn diagnostics_are_bounded() {
        match verdict("head -c 100000 /dev/zero | tr '\\0' x; exit 1").await {
            DoneVerdict::Retry(text) => assert!(text.len() <= MAX_DIAGNOSTIC_BYTES * 2),
            other => panic!("{other:?}"),
        }
    }

    async fn check_with_inherited_pipes(cancel_after_exit: bool) -> DoneVerdict {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = tokio::process::Command::new("/bin/bash");
        // Job control gives sleep its own process group, outside GroupGuard.
        cmd.args([
            "-c",
            "set -m; /bin/sleep 15 & echo $! > holder; echo $$ > shell",
        ]);
        let cancel = CancelToken::new();
        let context = ctx(cancel.clone());
        let env = HashMap::new();
        let timeout = if cancel_after_exit {
            CHECK_TIMEOUT
        } else {
            Duration::from_secs(1)
        };
        let check = run_process(cmd, dir.path(), &env, &context, "check", timeout);
        let observe_exit = async {
            let shell_path = dir.path().join("shell");
            loop {
                if let Some(pid) = std::fs::read_to_string(&shell_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                {
                    // SAFETY: signal zero probes whether the direct child is reaped.
                    if unsafe { libc::kill(pid, 0) } == -1 {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let pid = std::fs::read_to_string(dir.path().join("holder"))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap();
            if cancel_after_exit {
                cancel.cancel();
            }
            GroupGuard(Some(pid))
        };
        let (verdict, _holder) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(check, observe_exit)
        })
        .await
        .expect("pipe reader did not stop");
        verdict
    }

    #[tokio::test]
    async fn timeout_interrupts_pipe_collection_after_exit() {
        assert_eq!(
            check_with_inherited_pipes(false).await,
            DoneVerdict::Error("check timed out after 1s".into())
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_pipe_collection_after_exit() {
        assert_eq!(
            check_with_inherited_pipes(true).await,
            DoneVerdict::Error("check cancelled".into())
        );
    }

    #[tokio::test]
    async fn cancellation_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let entered = dir.path().join("entered");
        let marker = dir.path().join("alive");
        let cancel = CancelToken::new();
        let check = dsl_done_check(DslDoneParams {
            command: format!(
                "touch {}; (sleep 1; touch {}) & wait",
                entered.display(),
                marker.display()
            ),
            env: HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
        });
        let c = cancel.clone();
        let signal = entered.clone();
        tokio::spawn(async move {
            while !signal.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            c.cancel();
        });
        assert!(matches!(
            check.call(ctx(cancel)).await,
            DoneVerdict::Error(_)
        ));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "background child survived cancellation");
    }
}
