use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rig_core::completion::CompletionModel;

use super::tools::{self, ToolContext};

const MAX_DEPTH: u32 = 3;

/// Per-process monotonic source of subagent id segments. A clock-derived value
/// cannot separate siblings spawned microseconds apart, and duplicate segments
/// are exactly the log ambiguity this id exists to remove.
static SUBAGENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Segments are always four lowercase hex digits (`[sub.a3f1]`), so the counter
/// is truncated to its low 16 bits and ids repeat every 65 536 subagent
/// invocations. A repeat can only alias a sibling spawned that many invocations
/// later — no worse than the bare `[sub]` prefix this replaced.
fn next_segment() -> String {
    format!(
        "{:04x}",
        SUBAGENT_SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff
    )
}

/// Append a segment to the parent chain, so grandchildren carry full lineage.
fn extend_chain(parent: &str, seg: &str) -> String {
    if parent.is_empty() {
        seg.to_string()
    } else {
        format!("{parent}.{seg}")
    }
}

/// Chain of a subagent spawned under `parent`; empty parent means a first-level one.
fn child_chain(parent: &str) -> String {
    extend_chain(parent, &next_segment())
}

/// Log prefix of a subagent: `<base>[sub.<seg>[.<seg>…]] `, one 4-hex-digit
/// segment per nesting level.
fn subagent_prefix(base: &str, chain: &str) -> String {
    format!("{base}[sub.{chain}] ")
}

/// Build a subagent runner closure. Called once per backend before the agent loop.
/// The returned closure captures the model, tool filter, cancel token,
/// context prefix, and the original `ToolContext` — everything needed to run
/// a nested agent loop.
///
/// Recursive subagents are supported and bounded by `MAX_DEPTH`. Depth is a
/// true per-call-chain recursion bound, not a concurrency cap: each invocation
/// injects a child runner at `depth + 1` into the sub-context, so N sibling
/// subagents launched from one parent all share the same depth and never
/// exhaust the bound between them. Only genuine nesting increments depth.
pub(crate) fn make_runner<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: M,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    idle_timeout: f64,
    max_turns: usize,
) -> tools::SubagentFn {
    make_runner_at_depth(
        model,
        tool_filter,
        cancel,
        ctx,
        prefix,
        idle_timeout,
        max_turns,
        0,
        String::new(),
    )
}

#[allow(clippy::too_many_arguments)]
fn make_runner_at_depth<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: M,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    idle_timeout: f64,
    max_turns: usize,
    depth: u32,
    id_chain: String,
) -> tools::SubagentFn {
    Arc::new(move |task: String, cwd: Option<PathBuf>| {
        let model = model.clone();
        let tool_filter = tool_filter.clone();
        let cancel = cancel.clone();
        let mut sub_ctx = ctx.clone();
        let prefix = prefix.clone();
        let id_chain = id_chain.clone();

        if let Some(cwd) = cwd {
            sub_ctx.cwd = Some(cwd);
        }

        Box::pin(async move {
            if depth >= MAX_DEPTH {
                return format!("Error: subagent max depth ({MAX_DEPTH}) exceeded");
            }

            let new_chain = child_chain(&id_chain);
            let sub_prefix = subagent_prefix(&prefix, &new_chain);

            // Inject a child runner one level deeper so a nested subagent can
            // recurse again, bounded by MAX_DEPTH along this call chain.
            // Pass the original `prefix` (not `sub_prefix`) so prefixes don't
            // stack across nesting levels — each level appends its own id
            // segment to the chain instead.
            sub_ctx.subagent_fn = Some(make_runner_at_depth(
                model.clone(),
                tool_filter.clone(),
                cancel.clone(),
                sub_ctx.clone(),
                prefix.clone(),
                idle_timeout,
                max_turns,
                depth + 1,
                new_chain,
            ));

            let scratch = crate::config::scratch_dir(None)
                .unwrap_or_else(|| crate::config::scratch_root(None));
            let subagent_prompt = format!(
                "{}\n\n{}",
                crate::config::subagent_system_prompt(
                    &crate::config::work_root(),
                    &scratch,
                    &crate::config::project_root(),
                ),
                task,
            );

            let result = crate::clients::agent_loop::run_agent_loop_nested(
                &model,
                &subagent_prompt,
                &sub_ctx,
                &cancel,
                tool_filter.as_deref(),
                &sub_prefix,
                idle_timeout,
                max_turns,
            )
            .await;

            match result {
                Ok(completed) => completed.text_result.unwrap_or_default(),
                Err(e) => format!("Subagent error: {e}"),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;

    #[test]
    fn segments_are_four_hex_digits() {
        for seg in (0..2000).map(|_| next_segment()) {
            assert_eq!(seg.len(), 4, "segment {seg} must be four hex digits");
            assert!(
                seg.chars().all(|c| c.is_ascii_hexdigit()),
                "segment {seg} must be hex"
            );
        }
    }

    #[test]
    fn chain_appends_and_prefix_wraps() {
        assert_eq!(extend_chain("", "a"), "a");
        assert_eq!(extend_chain("a", "b"), "a.b");
        assert_eq!(extend_chain("a.b", "c"), "a.b.c");
        assert_eq!(child_chain("a.b").split('.').count(), 3);
        assert!(child_chain("a").starts_with("a."));
        assert_eq!(
            subagent_prefix("[base] ", "a3f1.b72e"),
            "[base] [sub.a3f1.b72e] "
        );
    }

    #[test]
    fn sibling_segments_do_not_collide_in_rapid_succession() {
        let segs: HashSet<String> = (0..1000).map(|_| child_chain("")).collect();
        assert_eq!(segs.len(), 1000, "sibling segments must be distinct");
    }

    fn depth_test_ctx() -> ToolContext {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-sub-depth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        ToolContext {
            cwd: Some(dir.clone()),
            extra_env: None,
            allowed_roots: vec![dir],
            audit_log: None,
            allowed_tools: None,
            subagent_fn: None,
            audit_lock: None,
        }
    }

    #[tokio::test]
    async fn make_runner_invokes_and_sets_subagent_fn() {
        let ctx = depth_test_ctx();

        // Model that returns a single text response in one turn.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("nested reply"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_runner(model, None, cancel, ctx, String::new(), 5.0, 10);

        // First invocation: depth 0 < 3, should succeed.
        let output = runner("first call".into(), None).await;
        assert!(
            !output.contains("max depth"),
            "depth 0 should not hit guard, got: {output}"
        );
    }

    /// A model whose stream never resolves — used to keep subagent calls
    /// in-flight so concurrent siblings overlap in time.
    #[derive(Clone)]
    struct PendingModel;

    impl rig_core::completion::CompletionModel for PendingModel {
        type Response = rig_core::test_utils::MockResponse;
        type StreamingResponse = rig_core::test_utils::MockResponse;
        type Client = ();

        fn make(_: &Self::Client, _: impl Into<String>) -> Self {
            Self
        }

        async fn completion(
            &self,
            _: rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::completion::CompletionResponse<Self::Response>,
            rig_core::completion::CompletionError,
        > {
            Err(rig_core::completion::CompletionError::ProviderError(
                "unused".into(),
            ))
        }

        async fn stream(
            &self,
            _: rig_core::completion::CompletionRequest,
        ) -> Result<
            rig_core::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
            rig_core::completion::CompletionError,
        > {
            let s: rig_core::streaming::StreamingResult<Self::StreamingResponse> =
                Box::pin(futures::stream::pending());
            Ok(rig_core::streaming::StreamingCompletionResponse::stream(s))
        }
    }

    /// Concurrent siblings share one depth level: N calls from the same parent
    /// must not exhaust the recursion bound between them.
    #[tokio::test]
    async fn make_runner_concurrent_siblings_do_not_exhaust_depth() {
        let ctx = depth_test_ctx();
        // Hangs forever so all siblings overlap in time.
        let model = PendingModel;
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_runner(model, None, cancel, ctx, String::new(), 0.2, 10);

        // Ten concurrent siblings at depth 0 — none should be rejected as
        // "max depth" even though they overlap in time.
        let handles: Vec<_> = (0..10)
            .map(|i| tokio::spawn(runner.clone()(format!("call {i}"), None)))
            .collect();
        for h in handles {
            let out = h.await.unwrap();
            assert!(
                !out.contains("max depth"),
                "sibling at depth 0 must not hit the recursion guard, got: {out}"
            );
        }
    }

    /// The recursion bound is enforced per call chain: a runner already at
    /// MAX_DEPTH rejects, standing in for a chain nested that many levels deep.
    #[tokio::test]
    async fn make_runner_rejects_at_max_depth() {
        let ctx = depth_test_ctx();
        let model = PendingModel;
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_runner_at_depth(
            model,
            None,
            cancel,
            ctx,
            String::new(),
            0.2,
            10,
            MAX_DEPTH,
            String::new(),
        );

        let blocked = runner("too deep".into(), None).await;
        assert!(
            blocked.contains("max depth (3) exceeded"),
            "call at MAX_DEPTH should be rejected, got: {blocked}"
        );
    }

    /// libtest replaces the test thread's stderr with an in-memory buffer, and
    /// `std::thread::spawn` propagates that capture to children, so `eprintln!`
    /// from the agent loop cannot be observed on fd 2 from a test thread. A raw
    /// pthread inherits no capture state, so the work runs there with fd 2
    /// pointed at a temp file. That needs POSIX fd redirection, so these tests
    /// are Unix-only as a unit; the format tests above stay portable.
    #[cfg(unix)]
    mod capture {
        use super::super::*;
        use super::*;

        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::sync::atomic::AtomicUsize;
        use std::sync::Mutex;

        /// Base prefix of the format tests, so their lines can be told apart
        /// from any other line that reaches the shared stderr capture.
        const TEST_BASE: &str = "[sub-test] ";

        static CAPTURE_SEQ: AtomicUsize = AtomicUsize::new(0);

        /// Panic captured off the raw-`pthread` job, re-raised on the test thread.
        type Panic = Box<dyn std::any::Any + Send>;

        static PANIC: Mutex<Option<Panic>> = Mutex::new(None);

        static JOB: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

        extern "C" fn run_job(_: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
            let job = JOB.lock().unwrap().take().expect("job queued before spawn");
            if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
                *PANIC.lock().unwrap() = Some(panic);
            }
            std::ptr::null_mut()
        }

        /// fd 2 is process-wide, so capturing tests take turns.
        static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

        fn capture_stderr(job: impl FnOnce() + Send + 'static) -> Vec<String> {
            let _serialized = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let path = std::env::temp_dir().join(format!(
                "gremlins-sub-log-{}-{}",
                std::process::id(),
                CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let file = std::fs::File::create(&path).unwrap();
            std::io::stderr().flush().unwrap();
            let saved = unsafe { libc::dup(2) };
            assert!(saved >= 0, "dup(stderr) failed");
            assert!(
                unsafe { libc::dup2(file.as_raw_fd(), 2) } >= 0,
                "dup2(stderr) failed"
            );

            *JOB.lock().unwrap() = Some(Box::new(job));
            let mut thread: libc::pthread_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::pthread_create(
                        &mut thread,
                        std::ptr::null(),
                        run_job,
                        std::ptr::null_mut(),
                    )
                },
                0,
                "pthread_create failed"
            );
            assert_eq!(
                unsafe { libc::pthread_join(thread, std::ptr::null_mut()) },
                0,
                "pthread_join failed"
            );

            std::io::stderr().flush().unwrap();
            unsafe { libc::dup2(saved, 2) };
            unsafe { libc::close(saved) };

            let mut buf = String::new();
            std::fs::File::open(&path)
                .unwrap()
                .read_to_string(&mut buf)
                .unwrap();
            let _ = std::fs::remove_file(&path);

            if let Some(panic) = PANIC.lock().unwrap().take() {
                std::panic::resume_unwind(panic);
            }
            buf.lines().map(str::to_string).collect()
        }

        /// `[sub.<chain>]` of every begin line this test's runner logged.
        fn sub_prefixes(lines: &[String]) -> Vec<String> {
            lines
                .iter()
                .filter(|l| l.contains(TEST_BASE) && l.contains("subagent: begin"))
                .map(|l| {
                    let chain = l.split_once("[sub.").expect("begin line carries an id").1;
                    format!("[sub.{}]", chain.split_once(']').unwrap().0)
                })
                .collect()
        }

        fn segments_of(prefixes: &[String]) -> Vec<String> {
            prefixes
                .iter()
                .map(|p| {
                    p.trim_start_matches("[sub.")
                        .trim_end_matches(']')
                        .to_string()
                })
                .collect()
        }

        fn block_on(fut: impl std::future::Future) {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(fut);
        }

        fn turn_text(text: &str) -> Vec<rig_core::test_utils::MockStreamEvent> {
            vec![
                rig_core::test_utils::MockStreamEvent::text(text),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]
        }

        fn turn_subagent_call() -> Vec<rig_core::test_utils::MockStreamEvent> {
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "subagent",
                    serde_json::json!({"task": "child task"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]
        }

        fn runner_with_turns(
            turns: Vec<Vec<rig_core::test_utils::MockStreamEvent>>,
        ) -> tools::SubagentFn {
            make_runner(
                rig_core::test_utils::MockCompletionModel::from_stream_turns(turns),
                None,
                super::super::super::agent_loop::CancelToken::new(),
                depth_test_ctx(),
                TEST_BASE.to_string(),
                5.0,
                10,
            )
        }

        #[test]
        fn sub_prefix_wraps_a_single_segment_at_depth_one() {
            let lines = capture_stderr(|| {
                let runner = runner_with_turns(vec![turn_text("only")]);
                block_on(runner("task".into(), None));
            });
            let segments = segments_of(&sub_prefixes(&lines));
            assert_eq!(segments.len(), 1, "get {segments:?} from {lines:?}");
            assert_eq!(
                segments[0].len(),
                4,
                "segment should be a four-digit id, got {segments:?}"
            );
            assert!(
                segments[0].chars().all(|c| c.is_ascii_hexdigit()),
                "segment should be an id, got {segments:?}"
            );
        }

        /// The child runner the parent injects is what a nested call runs, so
        /// its prefix must carry the parent's segment as lineage.
        #[test]
        fn nested_sub_prefix_appends_to_the_parent_chain() {
            let lines = capture_stderr(|| {
                let runner = runner_with_turns(vec![
                    turn_subagent_call(),
                    turn_text("leaf"),
                    turn_text("done"),
                ]);
                block_on(runner("task".into(), None));
            });
            let segments = segments_of(&sub_prefixes(&lines));
            assert_eq!(segments.len(), 2, "got {segments:?} from {lines:?}");
            let (parent, child) = (&segments[0], &segments[1]);
            assert_eq!(
                child.split('.').count(),
                2,
                "{child} should be a two-segment chain"
            );
            assert!(
                child.starts_with(&format!("{parent}.")),
                "child {child} should extend parent {parent}"
            );
        }

        /// Two invocations of the same runner are siblings: distinct ids, no
        /// shared parent segment.
        #[test]
        fn sibling_invocations_get_distinct_segments() {
            let lines = capture_stderr(|| {
                let runner = runner_with_turns(vec![turn_text("a"), turn_text("b")]);
                block_on(async {
                    runner("one".into(), None).await;
                    runner("two".into(), None).await;
                });
            });
            let segments = segments_of(&sub_prefixes(&lines));
            assert_eq!(segments.len(), 2, "got {segments:?} from {lines:?}");
            assert_ne!(segments[0], segments[1], "siblings must not share an id");
        }
    }
}
