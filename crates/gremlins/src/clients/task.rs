use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rig_core::completion::Message;

use super::model::GremlinsModel;
use super::task_kind::TaskKind;
use super::tools::{self, AgentLoopRuntime};

/// Build a [`TaskModelSelector`] from the raw `task-models` config map and a
/// provider-agnostic model factory.
///
/// Returns `None` when the map is empty (the common case). Resolution is
/// deliberately lazy: `factory` is only invoked for a matching task kind, so a
/// configured entry whose kind never runs does not force that provider's
/// backend or credentials to resolve.
pub(crate) fn build_task_selector(
    factory: TaskModelFactory,
    by_kind: &HashMap<TaskKind, String>,
) -> Option<TaskModelSelector> {
    let by_kind = by_kind.clone();
    TaskModelSelector::new(by_kind, factory)
}

const MAX_DEPTH: u32 = 3;

/// A model selected for a Task invocation, paired with the request options
/// resolved by the backend that built it.
///
/// `temperature`, `max_tokens`, and `extra` are the target provider's resolved
/// values — never the parent's — so a cross-provider override does not leak an
/// Anthropic `cache_control`/128 000-token default onto an OpenAI model.
#[derive(Clone, Debug)]
pub(crate) struct TaskModel {
    pub(crate) model: GremlinsModel,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) extra: Option<serde_json::Value>,
}

/// Builds the model named by a matching `task-models` entry, for use by one
/// Task invocation. Returns `Err` with a reason when the entry cannot be
/// resolved (for example, its backend cannot be built because a credential is
/// missing).
pub(crate) type TaskModelFactory = Arc<dyn Fn(&str) -> Result<TaskModel, String> + Send + Sync>;

/// Lookup map for `task-models`, keyed by the exact [`TaskKind`] spelling.
/// Shared behind an `Arc` so a Task fan-out clones one pointer per invocation
/// rather than the whole map.
#[derive(Debug)]
struct TaskModelOverrides {
    by_kind: HashMap<TaskKind, String>,
}

impl TaskModelOverrides {
    /// The spec configured for `kind`, if any. Lookup is an exact key match.
    fn spec_for(&self, kind: TaskKind) -> Option<&str> {
        self.by_kind.get(&kind).map(|s| s.as_str())
    }
}

/// Everything a Task invocation needs to pick a model from `task-models`: the
/// lookup maps and a factory that builds the model a matching entry names.
///
/// `None` at a call site means no `task-models` is configured, so every task
/// keeps the parent's model. Cloning a selector is O(1) — both halves sit behind
/// an `Arc` — which matters because a Task fan-out clones one per invocation.
#[derive(Clone)]
pub struct TaskModelSelector {
    overrides: Arc<TaskModelOverrides>,
    factory: TaskModelFactory,
}

impl std::fmt::Debug for TaskModelSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskModelSelector")
            .field("overrides", &self.overrides)
            .finish_non_exhaustive()
    }
}

impl TaskModelSelector {
    /// Assemble a selector from parsed config. Returns `None` when the map is
    /// empty — the common case, where nothing ever needs building.
    pub(crate) fn new(
        by_kind: HashMap<TaskKind, String>,
        factory: TaskModelFactory,
    ) -> Option<Self> {
        if by_kind.is_empty() {
            return None;
        }
        Some(Self {
            overrides: Arc::new(TaskModelOverrides { by_kind }),
            factory,
        })
    }

    /// The [`TaskModel`] for a Task of `kind`, or a clone of `default` when no
    /// entry matches.
    ///
    /// Returns `(model, spec_string)` where `spec_string` is the matched
    /// `task-models` entry (for logging), or `None` when no entry matched. A
    /// matched entry that fails to build is an `Err` — it must surface as a
    /// task result, not silently fall back to `default`.
    fn model_for(
        &self,
        kind: TaskKind,
        default: &TaskModel,
    ) -> Result<(TaskModel, Option<String>), String> {
        match self.overrides.spec_for(kind) {
            Some(spec) => match (self.factory)(spec) {
                Ok(m) => Ok((m, Some(spec.to_string()))),
                Err(reason) => Err(reason),
            },
            None => Ok((default.clone(), None)),
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
/// context prefix, and the original `AgentLoopRuntime` — everything needed to run
/// a nested agent loop.
///
/// `task_model_selector` carries the `task-models` configuration, so each
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
    model: GremlinsModel,
    task_model_selector: Option<TaskModelSelector>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: AgentLoopRuntime,
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
    model: GremlinsModel,
    task_model_selector: Option<TaskModelSelector>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<super::agent_loop::CancelToken>,
    ctx: AgentLoopRuntime,
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

                // A selector, when configured, may swap in a different model
                // based on this task's own `kind`; otherwise the parent's
                // stands. Fork always inherits the parent model directly and
                // never consults the task-models lookup.
                let parent_selection = TaskModel {
                    model: model.clone(),
                    temperature,
                    max_tokens,
                    extra: extra.clone(),
                };
                let selection = if kind == TaskKind::Fork {
                    Ok((parent_selection.clone(), None))
                } else {
                    match &task_model_selector {
                        Some(selector) => selector.model_for(kind, &parent_selection),
                        None => Ok((parent_selection.clone(), None)),
                    }
                };
                let (selected, _selected_spec) = match selection {
                    Ok(selection) => selection,
                    Err(reason) => return format!("Task error: {reason}"),
                };
                let selected_model = selected.model.clone();
                let selected_temperature = selected.temperature;
                let selected_max_tokens = selected.max_tokens;
                let selected_extra = selected.extra.clone();
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
                    new_chain.clone(),
                    completion_nudge_budget,
                    log_tx.clone(),
                    selected_temperature,
                    selected_max_tokens,
                    selected_extra.clone(),
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
                    &new_chain,
                    idle_timeout,
                    max_turns,
                    completion_nudge_budget,
                    log_tx.clone(),
                    selected_temperature,
                    selected_max_tokens,
                    selected_extra.clone(),
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
    use rig_core::driver::DynModel;
    use rig_core::operation::Completion;

    use std::collections::HashSet;

    fn gm(model: DynModel<Completion>) -> GremlinsModel {
        GremlinsModel::new(model, crate::config::DEFAULT_MAX_PROMPT_BYTES)
    }

    /// A [`TaskModel`] wrapping a mock completion model with no request
    /// options, for selector tests that distinguish the built model by its
    /// carried options.
    fn gm_mock_task() -> TaskModel {
        let model = rig_core::test_utils::MockCompletionModel::from_turns([]);
        TaskModel {
            model: gm(model.erase()),
            temperature: None,
            max_tokens: None,
            extra: None,
        }
    }

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
    fn task_model_overrides_match_by_kind() {
        let overrides = TaskModelOverrides {
            by_kind: HashMap::from([
                (TaskKind::Scout, "openai/mini".into()),
                (TaskKind::Implement, "openai/gpt-4o".into()),
            ]),
        };

        assert_eq!(overrides.spec_for(TaskKind::Scout), Some("openai/mini"));
        assert_eq!(
            overrides.spec_for(TaskKind::Implement),
            Some("openai/gpt-4o")
        );
        // Nothing matches.
        assert_eq!(overrides.spec_for(TaskKind::Review), None);
    }

    #[test]
    fn task_model_selector_builds_matched_model_or_defaults() {
        let target = TaskModel {
            model: gm(rig_core::test_utils::MockCompletionModel::from_turns([]).erase()),
            temperature: Some(0.2),
            max_tokens: Some(123),
            extra: Some(serde_json::json!({ "parallel_tool_calls": true })),
        };
        let factory: TaskModelFactory = Arc::new(move |spec: &str| {
            assert_eq!(spec, "openai/mini");
            Ok(target.clone())
        });
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".into())]),
            factory,
        )
        .expect("a non-empty task-models map yields a selector");

        let (selected, spec) = selector
            .model_for(TaskKind::Scout, &gm_mock_task())
            .unwrap();
        assert_eq!(spec.as_deref(), Some("openai/mini"));
        assert_eq!(selected.temperature, Some(0.2));
        assert_eq!(selected.max_tokens, Some(123));
        assert_eq!(
            selected.extra,
            Some(serde_json::json!({ "parallel_tool_calls": true }))
        );

        // No match — the default model and its options stand.
        let (selected, spec) = selector
            .model_for(TaskKind::Review, &gm_mock_task())
            .unwrap();
        assert_eq!(spec, None);
        assert_eq!(selected.temperature, None);
        assert_eq!(selected.max_tokens, None);
        assert_eq!(selected.extra, None);
    }

    #[test]
    fn task_model_selector_surfaces_factory_errors() {
        let factory: TaskModelFactory =
            Arc::new(|_spec: &str| Err("backend build failed: missing credential".to_string()));
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".into())]),
            factory,
        )
        .expect("a non-empty map yields a selector");

        // Spec matches but the factory fails — the error must surface.
        let err = selector
            .model_for(TaskKind::Scout, &gm_mock_task())
            .unwrap_err();
        assert!(
            err.contains("missing credential"),
            "factory error must be returned verbatim, got: {err}"
        );

        // No match still returns the default.
        let (selected, spec) = selector
            .model_for(TaskKind::Review, &gm_mock_task())
            .unwrap();
        assert_eq!(spec, None);
        assert_eq!(selected.temperature, None);
        assert_eq!(selected.max_tokens, None);
        assert_eq!(selected.extra, None);
    }

    #[test]
    fn task_model_selector_is_none_when_unconfigured() {
        let factory: TaskModelFactory = Arc::new(|_spec: &str| Err("unused".to_string()));
        assert!(TaskModelSelector::new(HashMap::new(), factory).is_none());
    }

    #[test]
    fn build_task_selector_does_not_eagerly_resolve_entries() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_flag = calls.clone();
        let factory: TaskModelFactory = Arc::new(move |_spec: &str| {
            calls_flag.fetch_add(1, Ordering::SeqCst);
            Err("unused".to_string())
        });
        let selector = build_task_selector(
            factory,
            &HashMap::from([(TaskKind::Scout, "openai/mini".into())]),
        )
        .expect("a non-empty task-models map yields a selector");

        // Construction alone must not resolve (and therefore not build) any
        // backend: the whole point of the factory is lazy credential access.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "selector construction must not resolve entries"
        );

        let result = selector.model_for(TaskKind::Scout, &gm_mock_task());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the matching lookup must resolve the entry exactly once"
        );
        assert!(result.is_err(), "factory error must surface");

        // A non-matching kind must not trigger another resolution.
        let (_selected, spec) = selector
            .model_for(TaskKind::Implement, &gm_mock_task())
            .unwrap();
        assert_eq!(spec, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a non-matching lookup must not resolve the factory"
        );
    }

    fn depth_test_ctx() -> AgentLoopRuntime {
        let dir = std::env::temp_dir().join(format!(
            "gremlins-sub-depth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        AgentLoopRuntime {
            cwd: Some(dir.clone()),
            extra_env: None,
            base_env: None,
            sandbox: tools::SandboxMode::Roots(vec![dir]),
            audit_log: None,
            allowed_tools: None,
            task_fn: None,
            audit_lock: None,
            parent_history: None,
            id_prefix: String::new(),
            telemetry: tools::AgentLoopTelemetry {
                stream_events: None,
                tool_output: None,
            },
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
            gm(model.clone().erase()),
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
    async fn oversized_task_prompt_returns_string_without_calling_model() {
        let ctx = depth_test_ctx();
        let inner = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("should not run"),
        ]]);
        let model = GremlinsModel::new(inner.clone().erase(), 16);
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

        let output = runner(TaskKind::Scout, "x".repeat(10_000), None).await;
        assert!(
            output.starts_with("Task error: prompt size "),
            "oversized task prompt must surface as a Task error string, got: {output}"
        );
        assert!(
            output.contains("exceeds configured limit of 16 bytes"),
            "oversized task prompt must report limit and actual size, got: {output}"
        );
        assert!(
            inner.requests().is_empty(),
            "oversized task prompt must not reach the model"
        );
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
            Err("factory must not run for Fork".to_string())
        });
        let selector = TaskModelSelector::new(
            HashMap::from([
                (TaskKind::Scout, "openai/mini".into()),
                (TaskKind::Fork, "openai/fork".into()),
            ]),
            factory,
        )
        .expect("a configured task-models map yields a selector");

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            gm(child.clone().erase()),
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
        // Fork must bypass task-models entirely, even when another kind is
        // configured with an override.
        assert!(
            !factory_called.load(Ordering::SeqCst),
            "Fork must never consult the task-models factory"
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

    /// A matched, non-fork override must run with the *target* model's own
    /// resolved request options — not the parent model's. This mirrors a
    /// cross-provider swap (e.g. an Anthropic parent delegating one task to
    /// `openai/gpt-4o`), where the parent's defaults would be invalid for the
    /// child provider.
    #[tokio::test]
    async fn task_override_uses_target_resolved_options() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let ctx = depth_test_ctx();

        let task_call = |id: &str| {
            MockStreamEvent::tool_call(
                id,
                "Task",
                serde_json::json!({"kind": "Scout", "prompt": "look"}),
            )
        };
        let done_call = |id: &str, summary: &str| {
            MockStreamEvent::tool_call(id, "Done", serde_json::json!({"summary": summary}))
        };

        let parent = MockCompletionModel::from_stream_turns([
            vec![
                task_call("t1"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                done_call("d1", "outer done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        // The overridden model is the one that actually answers the task.
        let child = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("scout result"),
            done_call("d2", "scout result"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);

        // The parent's options must not leak through the override.
        let parent_extra = serde_json::json!({ "cache_control": { "type": "ephemeral" } });
        let target_extra = serde_json::json!({ "parallel_tool_calls": true });
        let target_extra_for_factory = target_extra.clone();
        let child_handle = child.clone();
        let factory: TaskModelFactory = Arc::new(move |_spec: &str| {
            Ok(TaskModel {
                model: gm(child_handle.clone().erase()),
                temperature: Some(0.7),
                max_tokens: Some(8192),
                extra: Some(target_extra_for_factory.clone()),
            })
        });
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("configured");

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            gm(parent.clone().erase()),
            Some(selector),
            None,
            cancel,
            ctx,
            String::new(),
            "parent".into(),
            5.0,
            10,
            0,
            None,
            Some(0.1),
            Some(1111),
            Some(parent_extra.clone()),
        );

        let output = runner(TaskKind::Scout, "go".into(), None).await;
        assert!(!output.contains("Task error"), "unexpected error: {output}");

        for req in child.requests() {
            assert_eq!(
                req.temperature,
                Some(0.7),
                "nested turn must carry the target temperature, not the parent's"
            );
            assert_eq!(
                req.max_tokens,
                Some(8192),
                "nested turn must carry the target max_tokens, not the parent's"
            );
            assert_eq!(
                req.additional_params,
                Some(target_extra.clone()),
                "nested turn must carry the target passthrough, not the parent's"
            );
        }
    }

    /// When the factory fails to resolve a matched spec, the task must return
    /// a `Task error: …` string rather than quietly falling back to the parent
    /// model. The parent model is not invoked for that task.
    #[tokio::test]
    async fn task_build_failure_surfaces_as_task_error_without_invoking_parent() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let ctx = depth_test_ctx();
        let parent = MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("parent must not run"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);

        let factory: TaskModelFactory =
            Arc::new(|_spec: &str| Err("backend build failed: missing credential".to_string()));
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("configured");

        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner(
            gm(parent.clone().erase()),
            Some(selector),
            None,
            cancel,
            ctx,
            String::new(),
            "parent".into(),
            5.0,
            10,
            0,
            None,
            None,
            None,
            None,
        );

        let output = runner(TaskKind::Scout, "go".into(), None).await;
        assert!(
            output.starts_with("Task error: backend build failed"),
            "backend build failure must surface as a Task error string, got: {output}"
        );
        assert!(
            output.contains("missing credential"),
            "the reason must be included in the Task error, got: {output}"
        );
        assert!(
            parent.requests().is_empty(),
            "a failed task-models resolution must not invoke the parent model"
        );
    }

    #[tokio::test]
    async fn make_task_runner_rejects_at_max_depth() {
        let ctx = depth_test_ctx();
        let model = rig_core::test_utils::MockCompletionModel::from_turns([]).erase();
        let cancel = super::super::agent_loop::CancelToken::new();
        let runner = make_task_runner_at_depth(
            gm(model),
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
            gm(model),
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
    /// The inner agent loop retries 429s at the turn boundary (five slots in
    /// the default `turn-retry` schedule). After exhaustion the inner loop
    /// returns `RetryExhausted`, which is not retried again by the backend
    /// wrapper. Total: 6 requests (initial + 5 turn-level retries).
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
            gm(model.clone().erase()),
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
        tokio::time::advance(std::time::Duration::from_secs(200)).await;
        let output = handle.await.unwrap();
        assert!(
            output.starts_with("Task error: "),
            "exhausted retries must remain in the tool result, got: {output}"
        );
        // Inner loop: 5 turn-level retries (6 attempts) before giving up.
        assert_eq!(
            model.requests().len(),
            6,
            "turn-level retries exhaust after five backoff slots"
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
            gm(model),
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
                gm(rig_core::test_utils::MockCompletionModel::from_stream_turns(turns).erase()),
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
