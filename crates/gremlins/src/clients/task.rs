use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rig_core::completion::Message;
use rig_core::driver::DynModel;
use rig_core::operation::Completion;

use super::task_kind::TaskKind;
use super::tools::{self, ToolContext};

/// Build a [`TaskModelSelector`] from the raw `task-clients` config map and a
/// backend that can resolve `provider:model` specs into live models.
///
/// Returns `None` when the map is empty (the common case) or when the backend
/// cannot serve any of the configured entries.
pub(crate) fn build_task_selector(
    backend: std::sync::Arc<dyn crate::clients::backend::Backend>,
    by_kind: &HashMap<TaskKind, String>,
) -> Option<TaskModelSelector<DynModel<Completion>>> {
    if by_kind.is_empty() {
        return None;
    }
    // Verify the backend can resolve at least one configured entry.
    let any_resolvable = by_kind
        .values()
        .any(|spec| backend.make_model(spec).is_some());
    if !any_resolvable {
        return None;
    }
    let by_kind = by_kind.clone();
    TaskModelSelector::new(
        by_kind,
        Arc::new(move |spec: &str| backend.make_model(spec)),
    )
}

const MAX_DEPTH: u32 = 3;

/// Builds the model named by a matching `task-clients` entry, for use by one
/// Task invocation.  Returns `None` when the spec names a provider this
/// backend does not serve — the caller falls back to the parent's model.
pub(crate) type TaskModelFactory<M = DynModel<Completion>> =
    Arc<dyn Fn(&str) -> Option<M> + Send + Sync>;

/// Lookup map for `task-clients`, keyed by the exact [`TaskKind`] spelling.
/// Shared behind an `Arc` so a Task fan-out clones one pointer per invocation
/// rather than the whole map.
#[derive(Debug)]
struct TaskClientOverrides {
    by_kind: HashMap<TaskKind, String>,
}

impl TaskClientOverrides {
    /// The spec configured for `kind`, if any. Lookup is an exact key match.
    fn spec_for(&self, kind: TaskKind) -> Option<&str> {
        self.by_kind.get(&kind).map(|s| s.as_str())
    }
}

/// Everything a Task invocation needs to pick a model from `task-clients`: the
/// lookup maps and a factory that builds the model a matching entry names.
///
/// `None` at a call site means no `task-clients` is configured, so every task
/// keeps the parent's model. Cloning a selector is O(1) — both halves sit behind
/// an `Arc` — which matters because a Task fan-out clones one per invocation.
#[derive(Clone)]
pub struct TaskModelSelector<M = DynModel<Completion>> {
    overrides: Arc<TaskClientOverrides>,
    factory: TaskModelFactory<M>,
}

impl<M> std::fmt::Debug for TaskModelSelector<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskModelSelector")
            .field("overrides", &self.overrides)
            .finish_non_exhaustive()
    }
}

impl<M: Clone> TaskModelSelector<M> {
    /// Assemble a selector from parsed config. Returns `None` when the map is
    /// empty — the common case, where nothing ever needs building.
    pub(crate) fn new(
        by_kind: HashMap<TaskKind, String>,
        factory: TaskModelFactory<M>,
    ) -> Option<Self> {
        if by_kind.is_empty() {
            return None;
        }
        Some(Self {
            overrides: Arc::new(TaskClientOverrides { by_kind }),
            factory,
        })
    }

    /// The model for a Task of `kind`, or a clone of `default_model` when no
    /// entry matches or when the matching entry names a provider this backend
    /// cannot serve.
    ///
    /// Returns `(model, spec_string)` where `spec_string` is the matched
    /// `task-clients` entry (for logging), or `None` when no entry matched or
    /// the factory could not build a model for the matched spec.
    fn model_for(&self, kind: TaskKind, default_model: &M) -> (M, Option<String>)
    where
        M: Clone,
    {
        match self.overrides.spec_for(kind) {
            Some(spec) => match (self.factory)(spec) {
                Some(m) => (m, Some(spec.to_string())),
                None => (default_model.clone(), None),
            },
            None => (default_model.clone(), None),
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
/// invocation resolves its own model from its own `kind`. `None` means
/// no overrides are configured.
///
/// Recursive tasks are supported and bounded by `MAX_DEPTH`. Depth is a
/// true per-call-chain recursion bound, not a concurrency cap: each invocation
/// injects a child runner at `depth + 1` into the sub-context, so N sibling
/// tasks launched from one parent all share the same depth and never
/// exhaust the bound between them. Only genuine nesting increments depth.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_task_runner(
    model: DynModel<Completion>,
    task_model_selector: Option<TaskModelSelector<DynModel<Completion>>>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    parent_model_name: String,
    idle_timeout: f64,
    max_turns: usize,
    completion_nudge_budget: usize,
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    extra: Option<serde_json::Value>,
) -> tools::TaskFn {
    make_task_runner_at_depth(
        model,
        task_model_selector,
        tool_filter,
        cancel,
        ctx,
        prefix,
        parent_model_name,
        idle_timeout,
        max_turns,
        0,
        String::new(),
        completion_nudge_budget,
        log_tx,
        temperature,
        max_tokens,
        extra,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_task_runner_at_depth(
    model: DynModel<Completion>,
    task_model_selector: Option<TaskModelSelector<DynModel<Completion>>>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: ToolContext,
    prefix: String,
    _parent_model_name: String,
    idle_timeout: f64,
    max_turns: usize,
    depth: u32,
    id_chain: String,
    completion_nudge_budget: usize,
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    extra: Option<serde_json::Value>,
) -> tools::TaskFn {
    Arc::new(
        move |kind: TaskKind, task: String, parent_history: Option<Vec<Message>>| {
            let model = model.clone();
            let task_model_selector = task_model_selector.clone();
            let tool_filter = tool_filter.clone();
            let cancel = cancel.clone();
            let mut child_ctx = ctx.clone();
            let prefix = prefix.clone();
            let id_chain = id_chain.clone();
            let extra = extra.clone();

            let task_cwd = ctx.cwd.clone();
            let log_tx = log_tx.clone();

            Box::pin(async move {
                if depth >= MAX_DEPTH {
                    return format!("Error: task max depth ({MAX_DEPTH}) exceeded");
                }

                // A selector, when configured, may swap in a different model based
                // on this task's own `kind`; otherwise the parent's stands.
                // Fork always inherits the parent model directly and never consults
                // the task-clients lookup.
                let (selected_model, _selected_spec) = if kind == TaskKind::Fork {
                    (model.clone(), None)
                } else {
                    match &task_model_selector {
                        Some(selector) => selector.model_for(kind, &model),
                        None => (model.clone(), None),
                    }
                };
                // Use the model's own id() when available; fall back to the
                // parent model name so that log lines always identify the
                // actual model sent to the API.
                let model_name = selected_model.id().unwrap_or("?").to_string();

                let new_chain = child_chain(&id_chain);
                // Build the child prefix: [model_name] goes after the stage
                // bracket and before the task-chain segment, e.g.
                //   [stage][gpt-4o][task.a3f1]
                let child_prefix =
                    task_prefix(&format!("{}[{model_name}]", prefix.trim_end()), &new_chain);

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
                    model_name.clone(),
                    idle_timeout,
                    max_turns,
                    depth + 1,
                    new_chain,
                    completion_nudge_budget,
                    log_tx.clone(),
                    temperature,
                    max_tokens,
                    extra.clone(),
                ));

                let work_root = tools::worktree_root(task_cwd.as_deref());
                let scratch = tools::scratch_root().unwrap_or_else(|| work_root.clone());
                let system_prompt = if kind == TaskKind::Fork {
                    Some(crate::clients::config::fork_system_prompt(
                        &work_root, &scratch,
                    ))
                } else {
                    Some(crate::clients::config::task_system_prompt(
                        &work_root, &scratch,
                    ))
                };

                let prepopulated_messages = if kind == TaskKind::Fork {
                    parent_history
                } else {
                    None
                };

                let result = crate::clients::agent_loop::run_agent_loop_nested(
                    selected_model.clone(),
                    &task,
                    system_prompt.clone(),
                    &child_ctx,
                    &cancel,
                    tool_filter.as_deref(),
                    &child_prefix,
                    idle_timeout,
                    max_turns,
                    completion_nudge_budget,
                    log_tx.clone(),
                    temperature,
                    max_tokens,
                    extra.clone(),
                    prepopulated_messages,
                )
                .await;

                match result {
                    Ok(completed) => {
                        let out = completed.text_result.unwrap_or_default();
                        log::info!(
                            "task complete: kind={kind} prompt_len={p_len} prompt_preview={p_preview:?} output_len={o_len} output_preview={o_preview:?} has_done_header={has_done} has_md_header={has_md}",
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
        },
    )
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
    fn task_client_overrides_match_by_kind() {
        let overrides = TaskClientOverrides {
            by_kind: HashMap::from([
                (TaskKind::Scout, "openai:mini".into()),
                (TaskKind::Implement, "openai:gpt-4o".into()),
            ]),
        };

        assert_eq!(overrides.spec_for(TaskKind::Scout), Some("openai:mini"));
        assert_eq!(
            overrides.spec_for(TaskKind::Implement),
            Some("openai:gpt-4o")
        );
        // Nothing matches.
        assert_eq!(overrides.spec_for(TaskKind::Review), None);
    }

    #[test]
    fn task_model_selector_builds_matched_model_or_defaults() {
        let factory: TaskModelFactory<String> =
            Arc::new(|spec: &str| Some(format!("built:{spec}")));
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai:mini".into())]),
            factory,
        )
        .expect("a non-empty task-clients map yields a selector");

        assert_eq!(
            selector.model_for(TaskKind::Scout, &"default".to_string()),
            (
                "built:openai:mini".to_string(),
                Some("openai:mini".to_string())
            )
        );
        // No match — the default model stands.
        assert_eq!(
            selector.model_for(TaskKind::Review, &"default".to_string()),
            ("default".to_string(), None)
        );
    }

    #[test]
    fn task_model_selector_falls_back_when_factory_returns_none() {
        let factory: TaskModelFactory<String> = Arc::new(|_spec: &str| None);
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai:mini".into())]),
            factory,
        )
        .expect("a non-empty map yields a selector");

        // Spec matches but factory returns None — fall back to default.
        assert_eq!(
            selector.model_for(TaskKind::Scout, &"default".to_string()),
            ("default".to_string(), None)
        );
    }

    #[test]
    fn task_model_selector_is_none_when_unconfigured() {
        let factory: TaskModelFactory<String> = Arc::new(|_: &str| Some(String::new()));
        assert!(TaskModelSelector::new(HashMap::new(), factory).is_none());
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
            base_env: None,
            sandbox: tools::SandboxMode::Roots(vec![dir]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
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
            model.clone().erase(),
            None,
            None,
            cancel,
            ctx,
            String::new(),
            "model".into(),
            5.0,
            10,
            0,
            None,
            None,
            None,
            None,
        );

        // First invocation: depth 0 < 3, should succeed.
        let output = runner(TaskKind::Scout, "first call".into(), None).await;
        assert!(
            !output.contains("max depth"),
            "depth 0 should not hit guard, got: {output}"
        );

        // The task harness prompt must reach the model as a leading system message.
        for req in model.requests() {
            match req.chat_history.first() {
                Some(Message::System { content }) => assert!(
                    content.contains("<tools>"),
                    "unexpected system prompt: {content}"
                ),
                other => panic!("task must inject a system prompt, got: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn fork_inherits_parent_history_and_parent_model() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let ctx = depth_test_ctx();
        let child = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("fork reply"),
            MockStreamEvent::tool_call(
                "done1",
                "Done",
                serde_json::json!({"summary": "fork reply"}),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let factory_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let factory_flag = factory_called.clone();
        let factory: TaskModelFactory = Arc::new(move |_spec: &str| {
            factory_flag.store(true, Ordering::SeqCst);
            None
        });
        let selector = TaskModelSelector::new(
            HashMap::from([
                (TaskKind::Scout, "openai:mini".into()),
                (TaskKind::Fork, "openai:fork".into()),
            ]),
            factory,
        )
        .expect("a configured task-clients map yields a selector");

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            child.clone().erase(),
            Some(selector),
            None,
            cancel,
            ctx,
            String::new(),
            "model".into(),
            5.0,
            10,
            0,
            None,
            None,
            None,
            None,
        );

        let parent_history = vec![Message::user("earlier parent turn".to_string())];
        let output = runner(TaskKind::Fork, "child task".into(), Some(parent_history)).await;

        assert_eq!(output, "fork reply");
        // Fork must bypass task-clients entirely, even when another kind is
        // configured with an override.
        assert!(
            !factory_called.load(Ordering::SeqCst),
            "Fork must never consult the task-clients factory"
        );

        // The child's first request must carry the inherited parent turn
        // followed by the fork prompt as the next user message.
        let requests = child.requests();
        let child_history = format!("{:?}", requests[0].chat_history);
        assert!(
            child_history.contains("earlier parent turn"),
            "fork child must be seeded with parent history, got: {child_history}"
        );
        assert!(
            child_history.contains("child task"),
            "fork prompt must follow inherited history, got: {child_history}"
        );
    }

    #[tokio::test]
    async fn make_task_runner_rejects_at_max_depth() {
        let ctx = depth_test_ctx();
        let model = rig_core::test_utils::MockCompletionModel::from_turns([]).erase();
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner_at_depth(
            model,
            None,
            None,
            cancel,
            ctx,
            String::new(),
            "model".into(),
            0.2,
            10,
            MAX_DEPTH,
            String::new(),
            0,
            None,
            None,
            None,
            None,
        );

        let blocked = runner(TaskKind::Scout, "too deep".into(), None).await;
        assert!(
            blocked.contains("max depth (3) exceeded"),
            "call at MAX_DEPTH should be rejected, got: {blocked}"
        );
    }

    /// Retry a transient nested-loop failure; a 429 on the first attempt
    /// must back off and try again, yielding the successful text.
    #[tokio::test(start_paused = true)]
    async fn make_task_runner_retries_transient_errors() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let ctx = depth_test_ctx();
        let model = MockCompletionModel::from_stream_turns([
            vec![MockStreamEvent::error("http://429: slow down")],
            vec![
                MockStreamEvent::text("recovered"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ])
        .erase();
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            model,
            None,
            None,
            cancel,
            ctx,
            String::new(),
            "model".into(),
            5.0,
            10,
            0,
            None,
            None,
            None,
            None,
        );

        let handle = tokio::spawn(runner(TaskKind::Scout, "transient".into(), None));
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        let output = handle.await.unwrap();
        assert!(
            !output.contains("Task error"),
            "transient error must be retried, got: {output}"
        );
        assert_eq!(output, "recovered");
    }

    /// When every attempt fails retryably, the Task still returns a string
    /// result (not an unwinding error) so the parent sees `Task error: …`.
    ///
    /// The inner agent loop retries 429s at the turn boundary (3 slots in
    /// `TURN_RETRY_BACKOFF`). After exhaustion the inner loop returns
    /// `RetryExhausted`, which is *not* retryable by the outer `with_retry` — the
    /// backend-level wrapper no longer second-guesses exhausted turn-level
    /// retries. Total: 4 requests (initial + 3 turn-level retries).
    #[tokio::test(start_paused = true)]
    async fn make_task_runner_exhausts_retries_as_string() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let ctx = depth_test_ctx();
        let model = MockCompletionModel::from_stream_turns([
            vec![MockStreamEvent::error("http://429: slow down")],
            vec![MockStreamEvent::error("http://429: slow down")],
        ]);
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            model.clone().erase(),
            None,
            None,
            cancel,
            ctx,
            String::new(),
            "model".into(),
            5.0,
            10,
            0,
            None,
            None,
            None,
            None,
        );

        let handle = tokio::spawn(runner(TaskKind::Scout, "exhaust".into(), None));
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        let output = handle.await.unwrap();
        assert!(
            output.starts_with("Task error: "),
            "exhausted retries must remain in the tool result, got: {output}"
        );
        // Inner loop: 3 turn-level retries (4 attempts) before giving up.
        // The exhausted error is RetryExhausted, so with_retry does not retry.
        assert_eq!(
            model.requests().len(),
            4,
            "turn-level retries exhaust without with_retry safety net"
        );
    }

    /// A transport whose frames never resolve — the stream hangs forever.
    #[derive(Clone)]
    struct PendingTransport;

    impl rig_core::driver::Transport<rig_core::test_utils::MockScript> for PendingTransport {
        fn send(
            &self,
            _request: rig_core::completion::CompletionRequest,
            _exchange: rig_core::driver::Exchange,
        ) -> rig_core::driver::Opening<rig_core::test_utils::MockFrame> {
            rig_core::driver::Opening::ready(rig_core::driver::Opened::new(
                futures::stream::pending(),
            ))
        }
    }

    /// Concurrent siblings share one depth level: N calls from the same parent
    /// must not exhaust the recursion bound between them.
    #[tokio::test]
    async fn make_task_runner_concurrent_siblings_do_not_exhaust_depth() {
        let ctx = depth_test_ctx();
        let model = rig_core::driver::Model::new(
            rig_core::test_utils::MockScript::default(),
            PendingTransport,
        )
        .erase();
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            model,
            None,
            None,
            cancel,
            ctx,
            String::new(),
            "model".into(),
            0.2,
            10,
            0,
            None,
            None,
            None,
            None,
        );

        // Ten concurrent siblings at depth 0 — none should be rejected as
        // "max depth" even though they overlap in time.
        let handles: Vec<_> = (0..10)
            .map(|i| tokio::spawn(runner.clone()(TaskKind::Scout, format!("call {i}"), None)))
            .collect();
        for h in handles {
            let out = h.await.unwrap();
            assert!(
                !out.contains("max depth"),
                "sibling at depth 0 must not hit the recursion guard, got: {out}"
            );
        }
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

        /// Filter token for format tests — used to tell which log lines
        /// belong to this runner. Passed as both the runner's prefix (its trailing
        /// space is trimmed during model-name insertion) and as a substring filter
        /// in [`task_prefixes`].
        const TEST_BASE: &str = "[task-test]";

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
                    serde_json::json!({"kind": "Scout", "prompt": "child task"}),
                ),
                rig_core::test_utils::MockStreamEvent::final_response_with_default_usage(),
            ]
        }

        fn runner_with_turns(
            turns: Vec<Vec<rig_core::test_utils::MockStreamEvent>>,
        ) -> (tools::TaskFn, tokio::sync::mpsc::UnboundedReceiver<String>) {
            let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel();
            let runner = make_task_runner(
                rig_core::test_utils::MockCompletionModel::from_stream_turns(turns).erase(),
                None,
                None,
                super::super::super::agent_loop::CancelToken::new(),
                depth_test_ctx(),
                TEST_BASE.to_string(),
                "model".into(),
                5.0,
                10,
                0,
                Some(log_tx),
                None,
                None,
                None,
            );
            (runner, log_rx)
        }

        /// Drain the log channel into a Vec of lines.
        fn drain_log(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
            let mut lines = Vec::new();
            while let Ok(line) = rx.try_recv() {
                lines.push(line);
            }
            lines
        }

        #[test]
        fn task_prefix_wraps_a_single_segment_at_depth_one() {
            let (runner, mut log_rx) = runner_with_turns(vec![turn_text("only")]);
            block_on(runner(TaskKind::Scout, "task".into(), None));
            let lines = drain_log(&mut log_rx);
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
            let (runner, mut log_rx) =
                runner_with_turns(vec![turn_task_call(), turn_text("leaf"), turn_text("done")]);
            block_on(runner(TaskKind::Scout, "task".into(), None));
            let lines = drain_log(&mut log_rx);
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
            let (runner, mut log_rx) = runner_with_turns(vec![turn_text("a"), turn_text("b")]);
            block_on(async {
                runner(TaskKind::Scout, "one".into(), None).await;
                runner(TaskKind::Scout, "two".into(), None).await;
            });
            let lines = drain_log(&mut log_rx);
            let segments = segments_of(&task_prefixes(&lines));
            assert_eq!(segments.len(), 2, "got {segments:?} from {lines:?}");
            assert_ne!(segments[0], segments[1], "siblings must not share an id");
        }
    }
}
