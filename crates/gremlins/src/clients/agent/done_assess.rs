//! Native assessment of a proposed `Done`: a built-in required-output
//! presence check followed by the stage's optional user function.

use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use crate::clients::cancel::CancelToken;
use crate::clients::error::ClientError;
use crate::stage_spec::done_check::{DoneCheck, DoneContext, DoneOutput, DoneVerdict};

use super::agent_loop::cancelled_error;

/// Execution-local data for assessing a stage's `Done`.
#[derive(Clone, Debug, Default)]
pub(crate) struct DoneAssessment {
    /// Required (non-optional) outputs, resolved to local write paths.
    pub(crate) outputs: Vec<DoneOutput>,
    /// The stage's custom assessor, if any.
    pub(crate) check: Option<DoneCheck>,
    /// Replaces the native presence check when set (DSL stages use the CLI).
    pub(crate) artifact: Option<DoneCheck>,
}

impl DoneAssessment {
    /// An assessment that only requires the given files to exist.
    pub(crate) fn from_paths(paths: &[std::path::PathBuf]) -> Option<Self> {
        (!paths.is_empty()).then(|| Self {
            outputs: paths
                .iter()
                .map(|path| DoneOutput {
                    key: path.display().to_string(),
                    uri: String::new(),
                    path: path.clone(),
                })
                .collect(),
            check: None,
            artifact: None,
        })
    }
}

/// The outcome of assessing a proposed `Done`.
pub(crate) enum Assessment {
    Accept,
    /// Unmet: the full text to return to the model.
    Retry(String),
}

fn operational(message: String) -> ClientError {
    ClientError::Runtime { message }
}

/// The required outputs that are not regular files.
///
/// A regular file (even an empty one) is present; absent, dangling and
/// non-file paths are unsatisfied; any other metadata error is operational.
/// Nothing is created and no contents are read.
#[cfg(test)]
pub(crate) fn unsatisfied(outputs: &[DoneOutput]) -> Result<Vec<&DoneOutput>, ClientError> {
    let mut missing = Vec::new();
    for output in outputs {
        if !present(output, std::fs::metadata(&output.path))? {
            missing.push(output);
        }
    }
    Ok(missing)
}

// Both the synchronous text-turn policy and the cancellable Done assessment
// classify filesystem results identically.
fn present(
    output: &DoneOutput,
    metadata: std::io::Result<std::fs::Metadata>,
) -> Result<bool, ClientError> {
    match metadata {
        Ok(meta) => Ok(meta.is_file()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(operational(format!(
            "cannot inspect required output {}: {e}",
            output.path.display()
        ))),
    }
}

pub(crate) fn artifact_reminder(missing: &[&DoneOutput]) -> String {
    let paths: Vec<String> = missing
        .iter()
        .map(|o| format!("  - {}", o.path.display()))
        .collect();
    format!(
        "The following expected output file(s) were not written:\n{}\n\
         Please write each file using the Write tool now. Do not explain \
         \u{2014} just write the files.",
        paths.join("\n")
    )
}

/// The reminder text for unmet required outputs, or `None` when satisfied.
/// Uses the stage's artifact predicate when present, else a file check.
pub(crate) async fn missing_outputs(
    done: &DoneAssessment,
    cwd: Option<&Path>,
    summary: &str,
    cancel: &Arc<CancelToken>,
) -> Result<Option<String>, ClientError> {
    if let Some(artifact) = &done.artifact {
        let verdict = artifact
            .call(DoneContext {
                summary: summary.to_string(),
                cwd: cwd.map(Path::to_path_buf).unwrap_or_default(),
                outputs: done.outputs.clone(),
                cancel: cancel.clone(),
            })
            .await;
        return match verdict {
            DoneVerdict::Accept => Ok(None),
            DoneVerdict::Retry(diagnostic) => Ok(Some(diagnostic)),
            DoneVerdict::Error(message) => {
                Err(operational(format!("artifact check failed: {message}")))
            }
        };
    }
    let mut missing = Vec::new();
    for output in &done.outputs {
        if !present(output, tokio::fs::metadata(&output.path).await)? {
            missing.push(output);
        }
    }
    Ok((!missing.is_empty()).then(|| artifact_reminder(&missing)))
}

/// Run the built-in output check, then the custom function if it passes.
/// Cancellation wins over a pending assessor, which is dropped.
pub(crate) async fn assess(
    done: &DoneAssessment,
    cwd: Option<&Path>,
    summary: &str,
    cancel: &CancelToken,
) -> Result<Assessment, ClientError> {
    let signal = CancelToken::new();
    let in_check = std::sync::atomic::AtomicBool::new(false);
    let assessment = async {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        if let Some(reminder) = missing_outputs(done, cwd, summary, &signal).await? {
            return Ok(Assessment::Retry(format!(
                "Done rejected: {reminder}\nWrite each file using the Write tool, then call Done."
            )));
        }
        let Some(check) = &done.check else {
            return Ok(Assessment::Accept);
        };
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        in_check.store(true, std::sync::atomic::Ordering::SeqCst);
        match check
            .call(DoneContext {
                summary: summary.to_string(),
                cwd: cwd.map(Path::to_path_buf).unwrap_or_default(),
                outputs: done.outputs.clone(),
                cancel: signal.clone(),
            })
            .await
        {
            DoneVerdict::Accept => Ok(Assessment::Accept),
            DoneVerdict::Retry(diagnostic) => {
                Ok(Assessment::Retry(format!("Done rejected: {diagnostic}")))
            }
            DoneVerdict::Error(message) => {
                Err(operational(format!("done check failed: {message}")))
            }
        }
    };
    cancellable(
        cancel,
        &signal,
        || grace_for(done, in_check.load(std::sync::atomic::Ordering::SeqCst)),
        assessment,
    )
    .await
}

/// Only DSL checks own child processes they reap on cancellation; native
/// callbacks and the built-in file check are signalled and dropped at once.
/// Only the currently active check (custom when `in_check`, else artifact)
/// decides whether cancellation waits.
pub(super) fn grace_for(done: &DoneAssessment, in_check: bool) -> std::time::Duration {
    let active = if in_check {
        &done.check
    } else {
        &done.artifact
    };
    let reaping = active.iter().any(DoneCheck::reaps_on_cancel);
    if reaping {
        CANCEL_GRACE
    } else {
        std::time::Duration::ZERO
    }
}

/// How long a cancelled check may take to stop its own work before it is
/// dropped; a native callback that ignores its token cannot stall the run.
pub(super) const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Drive `work`, which observes `signal`. On run cancellation the signal is
/// raised and the work is given a bounded grace period to stop and release
/// its resources (child processes, pending scopes) before cancellation is
/// reported; its result is discarded.
pub(crate) async fn cancellable<T>(
    cancel: &CancelToken,
    signal: &CancelToken,
    grace: impl Fn() -> std::time::Duration,
    work: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    tokio::pin!(work);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            signal.cancel();
            let grace = grace();
            if !grace.is_zero() {
                let _ = tokio::time::timeout(grace, &mut work).await;
            }
            Err(cancelled_error())
        }
        // Cancellation can also arrive during the work's final poll.
        result = &mut work => {
            if cancel.is_cancelled() {
                signal.cancel();
                Err(cancelled_error())
            } else {
                result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn output(path: std::path::PathBuf) -> DoneOutput {
        DoneOutput {
            key: "k".into(),
            uri: "artifact://k".into(),
            path,
        }
    }

    #[tokio::test]
    async fn missing_output_blocks_custom_check_and_empty_file_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let done = DoneAssessment {
            outputs: vec![output(path.clone())],
            artifact: None,
            check: Some(DoneCheck::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { DoneVerdict::Accept }
            })),
        };
        let cancel = CancelToken::new();
        let first = assess(&done, None, "s", &cancel).await.unwrap();
        assert!(matches!(first, Assessment::Retry(m) if m.contains("out.txt")));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        std::fs::write(&path, "").unwrap();
        let second = assess(&done, None, "s", &cancel).await.unwrap();
        assert!(matches!(second, Assessment::Accept));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_default_neither_accepts_retries_nor_returns_metadata_error() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("written");
        std::fs::write(&regular, "").unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        for path in [dir.path().join("missing"), regular.join("child"), regular] {
            let done = DoneAssessment {
                outputs: vec![output(path)],
                check: None,
                artifact: None,
            };
            assert!(matches!(
                assess(&done, None, "s", &cancel).await,
                Err(ClientError::Cancelled)
            ));
        }
    }

    #[test]
    fn cancellation_while_waiting_for_required_file_metadata() {
        // Hold the runtime's only blocking worker so the actual metadata
        // operation cannot finish between polling the assessment and cancelling.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let done = DoneAssessment {
                outputs: vec![output(dir.path().join("missing"))],
                check: None,
                artifact: None,
            };
            let cancel = CancelToken::new();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker = tokio::task::spawn_blocking(move || {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv();
            });
            entered_rx.await.unwrap();
            let assessment = assess(&done, None, "s", &cancel);
            tokio::pin!(assessment);
            assert!(futures::poll!(&mut assessment).is_pending());
            cancel.cancel();
            assert!(matches!(
                futures::poll!(&mut assessment),
                std::task::Poll::Ready(Err(ClientError::Cancelled))
            ));
            release_tx.send(()).unwrap();
            worker.await.unwrap();
        });
    }

    #[tokio::test]
    async fn cancellation_during_final_poll_wins_over_ready_verdict() {
        let cancel = CancelToken::new();
        let trigger = cancel.clone();
        let done = DoneAssessment {
            outputs: vec![],
            artifact: None,
            check: Some(DoneCheck::new(move |_| {
                let trigger = trigger.clone();
                async move {
                    trigger.cancel();
                    DoneVerdict::Accept
                }
            })),
        };
        assert!(matches!(
            assess(&done, None, "s", &cancel).await,
            Err(ClientError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_link_is_missing_and_other_metadata_errors_are_operational() {
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        let outputs = [output(dangling)];
        assert_eq!(unsatisfied(&outputs).unwrap().len(), 1);

        // A path through a regular file fails with NotADirectory: neither
        // present nor missing.
        let file = dir.path().join("plain");
        std::fs::write(&file, "x").unwrap();
        let outputs = [output(file.join("child"))];
        let err = unsatisfied(&outputs).unwrap_err().to_string();
        assert!(err.contains("cannot inspect"), "{err}");
    }

    #[tokio::test]
    async fn directory_is_not_a_file_and_error_is_operational() {
        let dir = tempfile::tempdir().unwrap();
        let outputs = [output(dir.path().to_path_buf())];
        assert_eq!(unsatisfied(&outputs).unwrap().len(), 1);
        let done = DoneAssessment {
            outputs: vec![],
            artifact: None,
            check: Some(DoneCheck::new(|_| async {
                DoneVerdict::Error("boom".into())
            })),
        };
        assert!(assess(&done, None, "s", &CancelToken::new()).await.is_err());
    }

    #[tokio::test]
    async fn noncooperative_native_check_is_not_awaited_despite_reaping_artifact() {
        let cancel = CancelToken::new();
        let done = DoneAssessment {
            outputs: vec![],
            artifact: Some(DoneCheck::new(|_| async { DoneVerdict::Accept }).reaping()),
            check: Some(DoneCheck::new(|_| async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                DoneVerdict::Accept
            })),
        };
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            c.cancel();
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            assess(&done, None, "s", &cancel),
        )
        .await
        .expect("native check must not be awaited");
        assert!(matches!(result, Err(ClientError::Cancelled)));
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_reaping_check_is_dropped_after_cancel_grace() {
        let done = DoneAssessment {
            outputs: vec![],
            artifact: None,
            check: Some(DoneCheck::new(|_| std::future::pending()).reaping()),
        };
        let cancel = CancelToken::new();
        let assessment = assess(&done, None, "s", &cancel);
        tokio::pin!(assessment);
        assert!(futures::poll!(&mut assessment).is_pending());
        cancel.cancel();
        assert!(futures::poll!(&mut assessment).is_pending());
        tokio::time::advance(CANCEL_GRACE).await;
        assert!(matches!(
            futures::poll!(&mut assessment),
            std::task::Poll::Ready(Err(ClientError::Cancelled))
        ));
    }

    #[tokio::test]
    async fn cancellation_waits_for_cooperative_cleanup() {
        let cancel = CancelToken::new();
        let signal = CancelToken::new();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let flag = cleaned.clone();
        let (s, c) = (signal.clone(), cancel.clone());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c.cancel();
        });
        let work = async move {
            s.cancelled().await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ClientError>(())
        };
        let result = cancellable(&cancel, &signal, || CANCEL_GRACE, work).await;
        assert!(matches!(result, Err(ClientError::Cancelled)));
        assert_eq!(
            cleaned.load(Ordering::SeqCst),
            1,
            "cleanup must finish first"
        );
    }

    #[tokio::test]
    async fn cancelling_a_running_dsl_check_stops_and_reaps_it_before_returning() {
        use crate::executor::dsl_done::{dsl_done_check, DslDoneParams};
        let dir = tempfile::tempdir().unwrap();
        let (started, pid, late) = (
            dir.path().join("started"),
            dir.path().join("pid"),
            dir.path().join("late"),
        );
        let command = format!(
            "echo $$ > {}; (sleep 1; touch {}) & touch {}; wait",
            pid.display(),
            late.display(),
            started.display()
        );
        let done = DoneAssessment {
            outputs: vec![],
            artifact: None,
            check: Some(dsl_done_check(DslDoneParams {
                command,
                env: [("PATH".to_string(), "/usr/bin:/bin".to_string())].into(),
            })),
        };
        let cancel = CancelToken::new();
        let trigger = cancel.clone();
        let flag = started.clone();
        tokio::spawn(async move {
            while !flag.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            trigger.cancel();
        });
        let began = std::time::Instant::now();
        let result = assess(&done, Some(dir.path()), "s", &cancel).await;
        assert!(matches!(result, Err(ClientError::Cancelled)));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));
        let pid: i32 = std::fs::read_to_string(&pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only probes for existence.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "process outlived return");
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(!late.exists(), "descendant wrote after cancellation");
    }
}
