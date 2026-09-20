use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rig_core::completion::CompletionModel;

use super::tools::{self, ToolContext};

const MAX_DEPTH: u32 = 3;

/// Builds the model named by a matching `task-clients` entry, for use by one
/// Task invocation.  Returns `None` when the spec names a provider this
/// backend does not serve — the caller falls back to the parent's model.
pub(crate) type TaskModelFactory<M> = Arc<dyn Fn(&str) -> Option<M> + Send + Sync>;

/// Lookup maps for `task-clients`, already lowercased at parse time. Shared
/// behind an `Arc` so a Task fan-out clones one pointer per invocation rather
/// than the whole map.
struct TaskClientOverrides {
    exact: HashMap<String, String>,
    prefix: HashMap<String, String>,
}

impl TaskClientOverrides {
    /// The spec configured for `description`, if any. An exact key wins over any
    /// prefix; among matching prefixes the longest wins. Lookup is
    /// case-insensitive — keys were lowercased at parse time, so only the
    /// description is folded here.
    fn spec_for(&self, description: &str) -> Option<&str> {
        let desc = description.to_lowercase();
        if let Some(spec) = self.exact.get(&desc) {
            return Some(spec);
        }
        self.prefix
            .iter()
            .filter(|(prefix, _)| desc.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, spec)| spec.as_str())
    }
}

/// Everything a Task invocation needs to pick a model from `task-clients`: the
/// lookup maps and a factory that builds the model a matching entry names.
///
/// `None` at a call site means no `task-clients` is configured, so every task
/// keeps the parent's model. Cloning a selector is O(1) — both halves sit behind
/// an `Arc` — which matters because a Task fan-out clones one per invocation.
#[derive(Clone)]
pub(crate) struct TaskModelSelector<M> {
    overrides: Arc<TaskClientOverrides>,
    factory: TaskModelFactory<M>,
}

impl<M> TaskModelSelector<M> {
    /// Assemble a selector from parsed config. Returns `None` when both maps are
    /// empty — the common case, where nothing ever needs building.
    pub(crate) fn new(
        exact: HashMap<String, String>,
        prefix: HashMap<String, String>,
        factory: TaskModelFactory<M>,
    ) -> Option<Self> {
        if exact.is_empty() && prefix.is_empty() {
            return None;
        }
        Some(Self {
            overrides: Arc::new(TaskClientOverrides { exact, prefix }),
            factory,
        })
    }

    /// The model for a Task described by `description`, or a clone of
    /// `default_model` when no entry matches or when the matching entry names
    /// a provider this backend cannot serve.
    fn model_for(&self, description: &str, default_model: &M) -> M
    where
        M: Clone,
    {
        match self.overrides.spec_for(description) {
            Some(spec) => match (self.factory)(spec) {
                Some(m) => m,
                None => default_model.clone(),
            },
            None => default_model.clone(),
        }
    }
}

/// Per-process monotonic source of task id segments. A clock-derived value
/// cannot separate siblings spawned microseconds apart, and duplicate segments
/// are exactly the log ambiguity this id exists to remove.
static TASK_SEQ: AtomicU64 = AtomicU64::new(0);

/// Segments are always four lowercase hex digits (`[task.a3f1]`), so the counter
/// draws from its low 16 bits and ids repeat every 65 536 task invocations. A
/// repeat can only alias a sibling spawned that many invocations later, which is
/// fine for runs well under that many tasks.
fn next_segment() -> String {
    format!("{:04x}", TASK_SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

/// Append a segment to the parent chain, so grandchildren carry full lineage.
fn extend_chain(parent: &str, seg: &str) -> String {
    if parent.is_empty() {
        seg.to_string()
    } else {
        format!("{parent}.{seg}")
    }
}

/// Chain of a task spawned under `parent`; empty parent means a first-level one.
fn child_chain(parent: &str) -> String {
    extend_chain(parent, &next_segment())
}

/// Log prefix of a task: `<base>[task.<seg>[.<seg>…]] `, one 4-hex-digit
/// segment per nesting level.
fn task_prefix(base: &str, chain: &str) -> String {
    format!("{base}[task.{chain}] ")
}

/// Build a task runner closure. Called once per backend before the agent loop.
/// The returned closure captures the model, tool filter, cancel token,
/// context prefix, and the original `ToolContext` — everything needed to run
/// a nested agent loop.
///
/// `task_model_selector` carries the `task-clients` configuration, so each
/// invocation resolves its own model from its own `description`. `None` means
/// no overrides are configured.
///
/// Recursive tasks are supported and bounded by `MAX_DEPTH`. Depth is a
/// true per-call-chain recursion bound, not a concurrency cap: each invocation
/// injects a child runner at `depth + 1` into the sub-context, so N sibling
/// tasks launched from one parent all share the same depth and never
/// exhaust the bound between them. Only genuine nesting increments depth.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_task_runner<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: M,
    task_model_selector: Option<TaskModelSelector<M>>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    idle_timeout: f64,
    max_turns: usize,
    completion_nudge_budget: usize,
) -> tools::TaskFn {
    make_task_runner_at_depth(
        model,
        task_model_selector,
        tool_filter,
        cancel,
        ctx,
        prefix,
        idle_timeout,
        max_turns,
        0,
        String::new(),
        completion_nudge_budget,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_task_runner_at_depth<M: CompletionModel + Clone + Send + Sync + 'static>(
    model: M,
    task_model_selector: Option<TaskModelSelector<M>>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    idle_timeout: f64,
    max_turns: usize,
    depth: u32,
    id_chain: String,
    completion_nudge_budget: usize,
) -> tools::TaskFn {
    Arc::new(move |description: String, task: String| {
        let model = model.clone();
        let task_model_selector = task_model_selector.clone();
        let tool_filter = tool_filter.clone();
        let cancel = cancel.clone();
        let mut child_ctx = ctx.clone();
        let prefix = prefix.clone();
        let id_chain = id_chain.clone();

        let task_cwd = ctx.cwd.clone();

        Box::pin(async move {
            if depth >= MAX_DEPTH {
                return format!("Error: task max depth ({MAX_DEPTH}) exceeded");
            }

            // A selector, when configured, may swap in a different model based
            // on this task's own `description`; otherwise the parent's stands.
            let selected_model = match &task_model_selector {
                Some(selector) => selector.model_for(&description, &model),
                None => model.clone(),
            };

            let new_chain = child_chain(&id_chain);
            let child_prefix = task_prefix(&prefix, &new_chain);

            // Inject a child runner one level deeper so a nested task can
            // recurse again, bounded by MAX_DEPTH along this call chain.
            // Pass the original `prefix` (not `child_prefix`) so prefixes don't
            // stack across nesting levels — each level appends its own id
            // segment to the chain instead.
            child_ctx.task_fn = Some(make_task_runner_at_depth(
                selected_model.clone(),
                task_model_selector.clone(),
                tool_filter.clone(),
                cancel.clone(),
                child_ctx.clone(),
                prefix.clone(),
                idle_timeout,
                max_turns,
                depth + 1,
                new_chain,
                completion_nudge_budget,
            ));

            let work_root = tools::worktree_root(task_cwd.as_deref());
            let scratch = tools::scratch_root().unwrap_or_else(|| work_root.clone());
            let system_prompt = Some(crate::clients::config::task_system_prompt(
                &work_root, &scratch,
            ));

            let result = crate::clients::agent_loop::run_agent_loop_nested(
                &selected_model,
                &task,
                system_prompt,
                &child_ctx,
                &cancel,
                tool_filter.as_deref(),
                &child_prefix,
                idle_timeout,
                max_turns,
                completion_nudge_budget,
            )
            .await;

            match result {
                Ok(completed) => {
                    let out = completed.text_result.unwrap_or_default();
                    let desc_preview = if description.is_empty() {
                        "(empty)".to_string()
                    } else {
                        tools::preview_str(&description, 80)
                    };
                    log::info!(
                        "task complete: desc={desc_q} prompt_len={p_len} prompt_preview={p_preview:?} output_len={o_len} output_preview={o_preview:?} has_done_header={has_done} has_md_header={has_md}",
                        desc_q = desc_preview,
                        p_len = task.len(),
                        p_preview = tools::preview_str(&task, 120),
                        o_len = out.len(),
                        o_preview = tools::preview_str(&out, 300),
                        has_done = out.starts_with("Done."),
                        has_md = out.starts_with("# "),
                    );
                    out
                }
                Err(e) => format!("Task error: {e}"),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::Message;

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
            task_prefix("[base] ", "a3f1.b72e"),
            "[base] [task.a3f1.b72e] "
        );
    }

    #[test]
    fn sibling_segments_do_not_collide_in_rapid_succession() {
        let segs: HashSet<String> = (0..1000).map(|_| child_chain("")).collect();
        assert_eq!(segs.len(), 1000, "sibling segments must be distinct");
    }

    #[test]
    fn task_client_overrides_match_case_insensitively() {
        let overrides = TaskClientOverrides {
            exact: HashMap::from([("scout".into(), "openai:mini".into())]),
            prefix: HashMap::from([
                ("sc".into(), "openai:sc".into()),
                ("impl".into(), "openai:short".into()),
                ("implement".into(), "openai:gpt-4o".into()),
            ]),
        };

        // An exact key wins over any matching prefix; the description is folded
        // so the key's case doesn't matter.
        assert_eq!(overrides.spec_for("Scout"), Some("openai:mini"));
        // Among matching prefixes the longest wins.
        assert_eq!(
            overrides.spec_for("Implementation of x"),
            Some("openai:gpt-4o")
        );
        // Nothing matches.
        assert_eq!(overrides.spec_for("review"), None);
    }

    #[test]
    fn task_model_selector_builds_matched_model_or_defaults() {
        let factory: TaskModelFactory<String> =
            Arc::new(|spec: &str| Some(format!("built:{spec}")));
        let selector = TaskModelSelector::new(
            HashMap::from([("scout".into(), "openai:mini".into())]),
            HashMap::new(),
            factory,
        )
        .expect("a non-empty task-clients map yields a selector");

        assert_eq!(
            selector.model_for("Scout", &"default".to_string()),
            "built:openai:mini"
        );
        // No match — the default model stands.
        assert_eq!(
            selector.model_for("review", &"default".to_string()),
            "default"
        );
    }

    #[test]
    fn task_model_selector_falls_back_when_factory_returns_none() {
        let factory: TaskModelFactory<String> = Arc::new(|_spec: &str| None);
        let selector = TaskModelSelector::new(
            HashMap::from([("scout".into(), "openai:mini".into())]),
            HashMap::new(),
            factory,
        )
        .expect("a non-empty map yields a selector");

        // Spec matches but factory returns None — fall back to default.
        assert_eq!(
            selector.model_for("Scout", &"default".to_string()),
            "default"
        );
    }

    #[test]
    fn task_model_selector_precedence_is_provider_agnostic() {
        // Precedence (exact > longest prefix) is resolved on raw entries
        // before the factory sees a spec.  A factory that only serves one
        // provider must not leak a lower-precedence match for another
        // provider when the winner is unsupported.
        let served = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory: TaskModelFactory<String> = {
            let served = served.clone();
            Arc::new(move |spec: &str| {
                served.lock().unwrap().push(spec.to_string());
                // Only serve "openai" provider.
                spec.strip_prefix("openai:")
                    .map(|model| format!("built:{model}"))
            })
        };
        let selector = TaskModelSelector::new(
            HashMap::from([("scout".into(), "openrouter:gpt-4o-mini".into())]),
            HashMap::from([("s".into(), "openai:gpt-4o".into())]),
            factory,
        )
        .expect("configured");

        // "Scout" has an exact match — the factory receives the openrouter
        // spec, returns None, and we fall back to default.  The lower-priority
        // prefix "s*" must not be exposed.
        let result = selector.model_for("Scout", &"default".to_string());
        assert_eq!(result, "default");
        assert_eq!(
            served.lock().unwrap().as_slice(),
            &["openrouter:gpt-4o-mini"],
            "only the winning spec should be presented to the factory"
        );
    }

    #[test]
    fn task_model_selector_is_none_when_unconfigured() {
        let factory: TaskModelFactory<String> = Arc::new(|_: &str| Some(String::new()));
        assert!(TaskModelSelector::new(HashMap::new(), HashMap::new(), factory).is_none());
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
            task_fn: None,
            audit_lock: None,
        }
    }

    #[tokio::test]
    async fn make_task_runner_invokes_and_sets_task_fn() {
        let ctx = depth_test_ctx();

        // Model that returns a single text response in one turn.
        let model = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("nested reply"),
            rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            model.clone(),
            None,
            None,
            cancel,
            ctx,
            String::new(),
            5.0,
            10,
            0,
        );

        // First invocation: depth 0 < 3, should succeed.
        let output = runner("label".into(), "first call".into()).await;
        assert!(
            !output.contains("max depth"),
            "depth 0 should not hit guard, got: {output}"
        );

        // The task harness prompt must reach the model as a leading system message.
        for req in model.requests() {
            match req.chat_history.first() {
                Message::System { content } => assert!(
                    content.contains("<tools>"),
                    "unexpected system prompt: {content}"
                ),
                other => panic!("task must inject a system prompt, got: {other:?}"),
            }
        }
    }

    /// A model whose stream never resolves — used to keep task calls
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
    async fn make_task_runner_concurrent_siblings_do_not_exhaust_depth() {
        let ctx = depth_test_ctx();
        // Hangs forever so all siblings overlap in time.
        let model = PendingModel;
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(model, None, None, cancel, ctx, String::new(), 0.2, 10, 0);

        // Ten concurrent siblings at depth 0 — none should be rejected as
        // "max depth" even though they overlap in time.
        let handles: Vec<_> = (0..10)
            .map(|i| tokio::spawn(runner.clone()(format!("label {i}"), format!("call {i}"))))
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
    async fn make_task_runner_rejects_at_max_depth() {
        let ctx = depth_test_ctx();
        let model = PendingModel;
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner_at_depth(
            model,
            None,
            None,
            cancel,
            ctx,
            String::new(),
            0.2,
            10,
            MAX_DEPTH,
            String::new(),
            0,
        );

        let blocked = runner("label".into(), "too deep".into()).await;
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
        use super::*;

        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::sync::atomic::AtomicUsize;
        use std::sync::Mutex;

        /// Base prefix of the format tests, so their lines can be told apart
        /// from any other line that reaches the shared stderr capture.
        const TEST_BASE: &str = "[task-test] ";

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

        /// Minimal logger that writes to stderr via eprintln! so the fd-2
        /// redirect in [`capture_stderr`] captures log output.
        struct TestLogger;

        impl log::Log for TestLogger {
            fn enabled(&self, _metadata: &log::Metadata) -> bool {
                true
            }
            fn log(&self, record: &log::Record) {
                if self.enabled(record.metadata()) {
                    eprintln!("{} {} {}", record.level(), record.target(), record.args());
                }
            }
            fn flush(&self) {
                let _ = std::io::stderr().flush();
            }
        }

        fn capture_stderr(job: impl FnOnce() + Send + 'static) -> Vec<String> {
            let _serialized = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            // Install a minimal logger that writes to stderr via eprintln! so
            // the fd-2 redirect in this function captures log output.
            let _ = log::set_logger(&TestLogger);
            log::set_max_level(log::LevelFilter::Info);
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

        /// `[task.<chain>]` of every begin line this test's runner logged.
        fn task_prefixes(lines: &[String]) -> Vec<String> {
            lines
                .iter()
                .filter(|l| l.contains(TEST_BASE) && l.contains("task: begin"))
                .map(|l| {
                    let chain = l.split_once("[task.").expect("begin line carries an id").1;
                    format!("[task.{}]", chain.split_once(']').unwrap().0)
                })
                .collect()
        }

        fn segments_of(prefixes: &[String]) -> Vec<String> {
            prefixes
                .iter()
                .map(|p| {
                    p.trim_start_matches("[task.")
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

        fn turn_task_call() -> Vec<rig_core::test_utils::MockStreamEvent> {
            vec![
                rig_core::test_utils::MockStreamEvent::tool_call(
                    "c1",
                    "Task",
                    serde_json::json!({"description": "child", "prompt": "child task"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]
        }

        fn runner_with_turns(
            turns: Vec<Vec<rig_core::test_utils::MockStreamEvent>>,
        ) -> tools::TaskFn {
            make_task_runner(
                rig_core::test_utils::MockCompletionModel::from_stream_turns(turns),
                None,
                None,
                super::super::super::agent_loop::CancelToken::new(),
                depth_test_ctx(),
                TEST_BASE.to_string(),
                5.0,
                10,
                0,
            )
        }

        #[test]
        fn task_prefix_wraps_a_single_segment_at_depth_one() {
            let lines = capture_stderr(|| {
                let runner = runner_with_turns(vec![turn_text("only")]);
                block_on(runner("label".into(), "task".into()));
            });
            let segments = segments_of(&task_prefixes(&lines));
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
        fn nested_task_prefix_appends_to_the_parent_chain() {
            let lines = capture_stderr(|| {
                let runner =
                    runner_with_turns(vec![turn_task_call(), turn_text("leaf"), turn_text("done")]);
                block_on(runner("label".into(), "task".into()));
            });
            let segments = segments_of(&task_prefixes(&lines));
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
                    runner("one".into(), "one".into()).await;
                    runner("two".into(), "two".into()).await;
                });
            });
            let segments = segments_of(&task_prefixes(&lines));
            assert_eq!(segments.len(), 2, "got {segments:?} from {lines:?}");
            assert_ne!(segments[0], segments[1], "siblings must not share an id");
        }
    }
}
