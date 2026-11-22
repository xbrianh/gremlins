use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::clients::path_context::PathContext;
use crate::clients::tools::{self, AgentLoopRuntime};
use crate::task_kind::TaskKind;
use rig_core::driver::DynModel;
use rig_core::operation::Completion;

pub(crate) use super::task_models::*;

const MAX_DEPTH: u32 = 3;

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
    model: DynModel<Completion>,
    task_model_selector: Option<TaskModelSelector>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<crate::clients::cancel::CancelToken>,
    ctx: AgentLoopRuntime,
    prefix: String,
    parent_model_name: String,
    idle_timeout: f64,
    max_turns: usize,
    completion_nudge_budget: usize,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    extra: Option<serde_json::Value>,
    preserve_reasoning_in_history: bool,
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
        temperature,
        max_tokens,
        extra,
        preserve_reasoning_in_history,
    )
}

#[allow(clippy::too_many_arguments)]
fn make_task_runner_at_depth(
    model: DynModel<Completion>,
    task_model_selector: Option<TaskModelSelector>,
    tool_filter: Option<Vec<String>>,
    cancel: Arc<crate::clients::cancel::CancelToken>,
    ctx: AgentLoopRuntime,
    prefix: String,
    _parent_model_name: String,
    idle_timeout: f64,
    max_turns: usize,
    depth: u32,
    id_chain: String,
    completion_nudge_budget: usize,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    extra: Option<serde_json::Value>,
    preserve_reasoning_in_history: bool,
) -> tools::TaskFn {
    Arc::new(
        move |kind: TaskKind,
              task: String,
              parent_conversation: Box<dyn crate::clients::conversation::Conversation>,
              parent_id: String,
              parent_call: String| {
            let model = model.clone();
            let task_model_selector = task_model_selector.clone();
            let tool_filter = tool_filter.clone();
            let cancel = cancel.clone();
            let mut child_ctx = ctx.clone();
            // Child workers never inherit the parent's resolved path cache: the
            // cache is keyed to the parent prompt and would short-circuit
            // discovery in a task whose context is entirely different.
            child_ctx.path_context = PathContext::default();
            let prefix = prefix.clone();
            let id_chain = id_chain.clone();
            let extra = extra.clone();
            let parent_id = parent_id.clone();
            let parent_call = parent_call.clone();

            let task_cwd = ctx.cwd.clone();

            Box::pin(async move {
                if depth >= MAX_DEPTH {
                    return tools::ToolOutcome::text(format!(
                        "Error: task max depth ({MAX_DEPTH}) exceeded"
                    ));
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
                    preserve_reasoning: None,
                };
                let selection = if kind == TaskKind::Fork {
                    Ok((parent_selection.clone(), None))
                } else {
                    match &task_model_selector {
                        Some(selector) => selector.model_for(kind, &parent_selection).await.map(
                            |(mut selected, spec)| {
                                if selected.preserve_reasoning.is_none() {
                                    selected.preserve_reasoning =
                                        Some(preserve_reasoning_in_history);
                                }
                                (selected, spec)
                            },
                        ),
                        None => Ok((parent_selection.clone(), None)),
                    }
                };
                let (selected, _selected_spec) = match selection {
                    Ok(selection) => selection,
                    Err(reason) => {
                        return tools::ToolOutcome::text(format!("Task error: {reason}"));
                    }
                };
                let selected_model = selected.model.clone();
                let selected_temperature = selected.temperature;
                let selected_max_tokens = selected.max_tokens;
                let selected_extra = selected.extra.clone();
                let child_preserve_reasoning = selected
                    .preserve_reasoning
                    .unwrap_or(preserve_reasoning_in_history);
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
                    selected_temperature,
                    selected_max_tokens,
                    selected_extra.clone(),
                    child_preserve_reasoning,
                ));

                let work_root = crate::core::paths::worktree_root(task_cwd.as_deref());
                let system_prompt = if kind == TaskKind::Fork {
                    Some(super::system_prompts::fork_system_prompt(&work_root))
                } else {
                    Some(super::system_prompts::task_system_prompt(&work_root))
                };

                let prepopulated_conversation = parent_conversation;

                let result = super::agent_loop::run_agent_loop_nested(
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
                    selected_temperature,
                    selected_max_tokens,
                    selected_extra.clone(),
                    child_preserve_reasoning,
                    prepopulated_conversation,
                    &parent_id,
                    &parent_call,
                    &kind.to_string(),
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
                        // A Scout/Implement child's Read/Grep evidence rides
                        // the Task tool result, so the parent keeps the proof
                        // after the child loop is gone. Fork children already
                        // inherited the parent transcript — no need to repeat it.
                        let output = if kind == TaskKind::Fork {
                            out
                        } else {
                            let evidence = completed
                                .conversation
                                .as_ref()
                                .map(|conv| conv.tool_evidence())
                                .unwrap_or_default();
                            if evidence.is_empty() {
                                out
                            } else {
                                format!("{out}\n\n{evidence}")
                            }
                        };
                        tools::ToolOutcome::text(output)
                    }
                    Err(e) => tools::ToolOutcome::text(format!("Task error: {e}")),
                }
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::conversation::ConversationStub;
    use rig_core::completion::Message;
    use std::collections::{HashMap, HashSet};

    /// A [`TaskModel`] wrapping a mock completion model with no request
    /// options, for selector tests that distinguish the built model by its
    /// carried options.
    fn gm_mock_task() -> TaskModel {
        let model = rig_core::test_utils::MockCompletionModel::from_turns([]);
        TaskModel {
            model: model.erase(),
            temperature: None,
            max_tokens: None,
            extra: None,
            preserve_reasoning: None,
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
            by_kind: HashMap::from([(TaskKind::Scout, "openai/mini".into())]),
        };

        assert_eq!(overrides.spec_for(TaskKind::Scout), Some("openai/mini"));
        // A configurable kind without an override falls through to `None`.
        assert_eq!(overrides.spec_for(TaskKind::Implement), None);
    }

    #[tokio::test]
    async fn task_model_selector_builds_matched_model_or_defaults() {
        let target = TaskModel {
            model: rig_core::test_utils::MockCompletionModel::from_turns([]).erase(),
            temperature: Some(0.2),
            max_tokens: Some(123),
            extra: Some(serde_json::json!({ "parallel_tool_calls": true })),
            preserve_reasoning: None,
        };
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(move |spec: &str| {
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
            .await
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
            .model_for(TaskKind::Implement, &gm_mock_task())
            .await
            .unwrap();
        assert_eq!(spec, None);
        assert_eq!(selected.temperature, None);
        assert_eq!(selected.max_tokens, None);
        assert_eq!(selected.extra, None);
    }

    #[tokio::test]
    async fn task_model_selector_surfaces_factory_errors() {
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(|_spec: &str| {
                Err("backend build failed: missing credential".to_string())
            });
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".into())]),
            factory,
        )
        .expect("a non-empty map yields a selector");

        // Spec matches but the factory fails — the error must surface.
        let err = selector
            .model_for(TaskKind::Scout, &gm_mock_task())
            .await
            .unwrap_err();
        assert!(
            err.contains("missing credential"),
            "factory error must be returned verbatim, got: {err}"
        );

        // No match still returns the default.
        let (selected, spec) = selector
            .model_for(TaskKind::Implement, &gm_mock_task())
            .await
            .unwrap();
        assert_eq!(spec, None);
        assert_eq!(selected.temperature, None);
        assert_eq!(selected.max_tokens, None);
        assert_eq!(selected.extra, None);
    }

    #[tokio::test]
    async fn yielding_factory_runs_only_on_match_and_surfaces_late_failure() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_flag = calls.clone();
        let factory: TaskModelFactory = Arc::new(move |_spec: &str| {
            calls_flag.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                tokio::task::yield_now().await;
                Err("late credential failure".to_string())
            })
        });
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".into())]),
            factory,
        )
        .unwrap();

        selector
            .model_for(TaskKind::Implement, &gm_mock_task())
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "unmatched kind skips factory"
        );

        let err = selector
            .model_for(TaskKind::Scout, &gm_mock_task())
            .await
            .unwrap_err();
        assert!(err.contains("late credential failure"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn task_model_selector_is_none_when_unconfigured() {
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(|_spec: &str| {
                Err("unused".to_string())
            });
        assert!(TaskModelSelector::new(HashMap::new(), factory).is_none());
    }

    #[tokio::test]
    async fn empty_replacement_selector_retains_lazy_fallback_on_clone() {
        let calls = Arc::new(AtomicU64::new(0));
        let flag = calls.clone();
        let factory = sync_factory(move |spec| {
            assert_eq!(spec, "effective/stage");
            flag.fetch_add(1, Ordering::SeqCst);
            Err("fallback credential failure".into())
        });
        let selector = TaskModelSelector::from_overrides(HashMap::new(), factory)
            .with_fallback_spec("effective/stage");
        let cloned = selector.clone();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for selector in [selector, cloned] {
            let err = selector
                .model_for(TaskKind::Implement, &gm_mock_task())
                .await
                .unwrap_err();
            assert_eq!(err, "fallback credential failure");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn build_task_selector_does_not_eagerly_resolve_entries() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_flag = calls.clone();
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(move |_spec: &str| {
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

        let result = selector.model_for(TaskKind::Scout, &gm_mock_task()).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the matching lookup must resolve the entry exactly once"
        );
        assert!(result.is_err(), "factory error must surface");

        // A non-matching kind must not trigger another resolution.
        let (_selected, spec) = selector
            .model_for(TaskKind::Implement, &gm_mock_task())
            .await
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
            id_prefix: String::new(),
            message_prefix: String::new(),
            discovered_skills: None,
            path_context: crate::clients::path_context::PathContext::default(),
            root_raw_stream_path: None,
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

        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        // First invocation: depth 0 < 3, should succeed.
        let output = runner(
            TaskKind::Scout,
            "first call".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        )
        .await;
        assert!(
            !output.output.contains("max depth"),
            "depth 0 should not hit guard, got: {}",
            output.output
        );

        // The task harness prompt must reach the model as a leading system message.
        for req in model.requests() {
            match req.chat_history.first().map(|m| m.as_ref()) {
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
        let _env = crate::test_support::EnvGuard::lock();
        crate::config::set_test_max_prompt_bytes(16);
        let ctx = depth_test_ctx();
        let inner = rig_core::test_utils::MockCompletionModel::from_stream_turns([vec![
            rig_core::test_utils::MockStreamEvent::text("should not run"),
        ]]);
        let model = inner.clone().erase();
        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        let output = runner(
            TaskKind::Scout,
            "x".repeat(10_000),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        )
        .await;
        assert!(
            output.output.starts_with("Task error: prompt size"),
            "oversized task prompt must be rejected before reaching the model, got: {}",
            output.output,
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
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(move |_spec: &str| {
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

        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        let parent_history = vec![Message::user("earlier parent turn")];
        let parent_conversation = Box::new(ConversationStub::with_history("", parent_history));
        let output = runner(
            TaskKind::Fork,
            "child task".into(),
            parent_conversation,
            String::new(),
            String::new(),
        )
        .await;

        assert_eq!(output.output, "fork reply");
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
        let _env = crate::test_support::EnvGuard::lock();
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
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(move |_spec: &str| {
                Ok(TaskModel {
                    model: child_handle.clone().erase(),
                    temperature: Some(0.7),
                    max_tokens: Some(8192),
                    extra: Some(target_extra_for_factory.clone()),
                    preserve_reasoning: None,
                })
            });
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("configured");

        let cancel = crate::clients::cancel::CancelToken::new();
        let runner = make_task_runner(
            parent.clone().erase(),
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
            Some(parent_extra.clone()),
            false,
        );

        let output = runner(
            TaskKind::Scout,
            "go".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        )
        .await;
        assert!(
            !output.output.contains("Task error"),
            "unexpected error: {}",
            output.output
        );

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

    /// Exercise selection through the Task tool installed on a mapped child,
    /// rather than calling model_for directly with a fabricated parent.
    async fn assert_recursive_task_selection(pinned_fallback: bool, grandchild_kind: TaskKind) {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

        let done_turn = |id: &str, summary: &str| {
            vec![
                MockStreamEvent::tool_call(id, "Done", serde_json::json!({"summary": summary})),
                MockStreamEvent::final_response_with_default_usage(),
            ]
        };
        let uses_stage = pinned_fallback && grandchild_kind != TaskKind::Fork;
        let mut mapped_turns = vec![vec![
            MockStreamEvent::text("mapped scout history sentinel"),
            MockStreamEvent::tool_call(
                "recursive-task",
                "Task",
                serde_json::json!({
                    "kind": grandchild_kind.to_string(),
                    "prompt": "grandchild prompt sentinel"
                }),
            ),
            MockStreamEvent::final_response_with_default_usage(),
        ]];
        if !uses_stage {
            mapped_turns.push(done_turn("grandchild-done", "grandchild result"));
        }
        mapped_turns.push(done_turn("scout-done", "scout result"));
        let mapped = MockCompletionModel::from_stream_turns(mapped_turns);
        let stage = MockCompletionModel::from_stream_turns(if uses_stage {
            vec![done_turn("stage-grandchild-done", "grandchild result")]
        } else {
            vec![]
        });
        let stage_extra = serde_json::json!({"stage_option": true});
        let mapped_extra = serde_json::json!({"mapped_option": true});
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_for_factory = calls.clone();
        let mapped_for_factory = mapped.clone();
        let stage_for_factory = stage.clone();
        let mapped_options = mapped_extra.clone();
        let stage_options = stage_extra.clone();
        let factory: TaskModelFactory =
            crate::clients::agent::task_models::sync_factory(move |spec: &str| {
                calls_for_factory.lock().unwrap().push(spec.to_owned());
                match spec {
                    "mapped/scout" => Ok(TaskModel {
                        model: mapped_for_factory.clone().erase(),
                        temperature: Some(0.7),
                        max_tokens: Some(8192),
                        extra: Some(mapped_options.clone()),
                        preserve_reasoning: None,
                    }),
                    "stage/model" => Ok(TaskModel {
                        model: stage_for_factory.clone().erase(),
                        temperature: Some(0.2),
                        max_tokens: Some(2048),
                        extra: Some(stage_options.clone()),
                        preserve_reasoning: None,
                    }),
                    other => panic!("unexpected model resolution: {other}"),
                }
            });
        let selector = TaskModelSelector::from_overrides(
            HashMap::from([(TaskKind::Scout, "mapped/scout".into())]),
            factory,
        );
        let selector = if pinned_fallback {
            selector.with_fallback_spec("stage/model")
        } else {
            selector
        };
        assert!(calls.lock().unwrap().is_empty(), "selection must be lazy");
        let runner = make_task_runner(
            stage.clone().erase(),
            Some(selector),
            None,
            crate::clients::cancel::CancelToken::new(),
            depth_test_ctx(),
            String::new(),
            "stage".into(),
            5.0,
            10,
            0,
            Some(0.2),
            Some(2048),
            Some(stage_extra.clone()),
            false,
        );
        let output = runner(
            TaskKind::Scout,
            "mapped scout prompt sentinel".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        )
        .await;
        assert_eq!(output.output, "scout result");
        let expected_calls = if uses_stage {
            vec!["mapped/scout", "stage/model"]
        } else {
            vec!["mapped/scout"]
        };
        assert_eq!(*calls.lock().unwrap(), expected_calls);
        let mapped_requests = mapped.requests();
        let stage_requests = stage.requests();
        assert_eq!(mapped_requests.len(), if uses_stage { 2 } else { 3 });
        assert_eq!(stage_requests.len(), usize::from(uses_stage));
        let resumed_history = format!("{:?}", mapped_requests.last().unwrap().chat_history);
        assert!(
            resumed_history.contains("grandchild result"),
            "grandchild result must reach the resumed Scout: {resumed_history}"
        );
        for req in &mapped_requests {
            assert_eq!(req.temperature, Some(0.7));
            assert_eq!(req.max_tokens, Some(8192));
            assert_eq!(req.additional_params, Some(mapped_extra.clone()));
        }
        for req in &stage_requests {
            assert_eq!(req.temperature, Some(0.2));
            assert_eq!(req.max_tokens, Some(2048));
            assert_eq!(req.additional_params, Some(stage_extra.clone()));
        }
        let grandchild_request = if uses_stage {
            &stage_requests[0]
        } else {
            &mapped_requests[1]
        };
        let history = format!("{:?}", grandchild_request.chat_history);
        assert!(history.contains("grandchild prompt sentinel"), "{history}");
        if grandchild_kind == TaskKind::Fork {
            assert!(
                history.contains("mapped scout prompt sentinel"),
                "{history}"
            );
            // The active Task turn is not committed yet; Fork inherits the
            // preceding conversation, including the mapped child's prompt.
        } else {
            assert!(
                !history.contains("mapped scout history sentinel"),
                "{history}"
            );
        }
    }

    #[tokio::test]
    async fn recursive_unmapped_task_inherits_mapped_parent_model_and_options() {
        assert_recursive_task_selection(false, TaskKind::Implement).await;
    }

    #[tokio::test]
    async fn recursive_unmapped_task_uses_pinned_stage_fallback_model_and_options() {
        assert_recursive_task_selection(true, TaskKind::Implement).await;
    }

    #[tokio::test]
    async fn recursive_fork_bypasses_pinned_fallback_and_inherits_mapped_history() {
        assert_recursive_task_selection(true, TaskKind::Fork).await;
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
            crate::clients::agent::task_models::sync_factory(|_spec: &str| {
                Err("backend build failed: missing credential".to_string())
            });
        let selector = TaskModelSelector::new(
            HashMap::from([(TaskKind::Scout, "openai/mini".to_string())]),
            factory,
        )
        .expect("configured");

        let cancel = crate::clients::cancel::CancelToken::new();
        let runner = make_task_runner(
            parent.clone().erase(),
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
            false,
        );

        let output = runner(
            TaskKind::Scout,
            "go".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        )
        .await;
        assert!(
            output
                .output
                .starts_with("Task error: backend build failed"),
            "backend build failure must surface as a Task error string, got: {}",
            output.output
        );
        assert!(
            output.output.contains("missing credential"),
            "the reason must be included in the Task error, got: {}",
            output.output
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
        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        let blocked = runner(
            TaskKind::Scout,
            "too deep".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        )
        .await;
        assert!(
            blocked.output.contains("max depth (3) exceeded"),
            "call at MAX_DEPTH should be rejected, got: {}",
            blocked.output
        );
    }

    /// Retry a transient nested-loop failure; a 429 on the first attempt
    /// must back off and try again, yielding the successful text.
    #[tokio::test(start_paused = true)]
    async fn make_task_runner_retries_transient_errors() {
        let _env = crate::test_support::EnvGuard::lock();
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
        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        let handle = tokio::spawn(runner(
            TaskKind::Scout,
            "transient".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        ));
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        let output = handle.await.unwrap();
        assert!(
            !output.output.contains("Task error"),
            "transient error must be retried, got: {}",
            output.output
        );
        assert_eq!(output.output, "recovered");
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
        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        let handle = tokio::spawn(runner(
            TaskKind::Scout,
            "exhaust".into(),
            Box::new(ConversationStub::new("")),
            String::new(),
            String::new(),
        ));
        tokio::time::advance(std::time::Duration::from_secs(200)).await;
        let output = handle.await.unwrap();
        assert!(
            output.output.starts_with("Task error: "),
            "exhausted retries must remain in the tool result, got: {}",
            output.output
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
        let cancel = crate::clients::cancel::CancelToken::new();
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
            false,
        );

        // Ten concurrent siblings at depth 0 — none should be rejected as
        // "max depth" even though they overlap in time.
        let handles: Vec<_> = (0..10)
            .map(|i| {
                tokio::spawn(runner.clone()(
                    TaskKind::Scout,
                    format!("call {i}"),
                    Box::new(ConversationStub::new("")),
                    String::new(),
                    String::new(),
                ))
            })
            .collect();
        for h in handles {
            let out = h.await.unwrap();
            assert!(
                !out.output.contains("max depth"),
                "sibling at depth 0 must not hit the recursion guard, got: {}",
                out.output
            );
        }
    }

    /// Prefix/lineage assertions that read the emitted `LogLine` bus events
    /// for a `[task.<chain>]`-formatted `task: begin` line.
    mod capture {
        use super::*;
        use gremlins_magic_bus::{connection::BusSubGuard, EventKind, ReceiverHalf};

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

        /// Per-test unique message prefix so parallel tests don't capture each
        /// other's bus log lines. Keeps `TEST_BASE` as a substring for filtering.
        fn unique_prefix() -> String {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            format!("{TEST_BASE}{}", SEQ.fetch_add(1, Ordering::Relaxed))
        }

        fn runner_with_turns(
            turns: Vec<Vec<rig_core::test_utils::MockStreamEvent>>,
        ) -> (tools::TaskFn, BusSubGuard, ReceiverHalf<EventKind>) {
            let mut ctx = depth_test_ctx();
            let prefix = unique_prefix();
            ctx.message_prefix = prefix.clone();
            let (sub, log_rx) = gremlins_magic_bus::subscribe_run(&prefix);
            let guard = BusSubGuard::new(sub);
            let runner = make_task_runner(
                rig_core::test_utils::MockCompletionModel::from_stream_turns(turns).erase(),
                None,
                None,
                crate::clients::cancel::CancelToken::new(),
                ctx,
                TEST_BASE.to_string(),
                "model".into(),
                5.0,
                10,
                0,
                None,
                None,
                None,
                false,
            );
            (runner, guard, log_rx)
        }

        /// Drain the captured `LogLine` bus events into a Vec.
        fn drain_log(rx: &mut ReceiverHalf<EventKind>) -> Vec<String> {
            let mut lines = Vec::new();
            while let Ok(dispatch) = rx.try_recv() {
                if let EventKind::LogLine { line, .. } = dispatch.kind() {
                    lines.push(line.clone());
                }
            }
            lines
        }

        #[test]
        fn task_prefix_wraps_a_single_segment_at_depth_one() {
            let (runner, _guard, mut log_rx) = runner_with_turns(vec![turn_text("only")]);
            block_on(runner(
                TaskKind::Scout,
                "task".into(),
                Box::new(ConversationStub::new("")),
                String::new(),
                String::new(),
            ));
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
            let (runner, _guard, mut log_rx) =
                runner_with_turns(vec![turn_task_call(), turn_text("leaf"), turn_text("done")]);
            block_on(runner(
                TaskKind::Scout,
                "task".into(),
                Box::new(ConversationStub::new("")),
                String::new(),
                String::new(),
            ));
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
            let (runner, _guard, mut log_rx) =
                runner_with_turns(vec![turn_text("a"), turn_text("b")]);
            block_on(async {
                runner(
                    TaskKind::Scout,
                    "one".into(),
                    Box::new(ConversationStub::new("")),
                    String::new(),
                    String::new(),
                )
                .await;
                runner(
                    TaskKind::Scout,
                    "two".into(),
                    Box::new(ConversationStub::new("")),
                    String::new(),
                    String::new(),
                )
                .await;
            });
            let lines = drain_log(&mut log_rx);
            let segments = segments_of(&task_prefixes(&lines));
            assert_eq!(segments.len(), 2, "got {segments:?} from {lines:?}");
            assert_ne!(segments[0], segments[1], "siblings must not share an id");
        }
    }

    /// Tests for per-task raw stream files.
    mod raw_stream {
        use super::*;
        use crate::clients::agent::agent_loop::{
            loop_opts, run_agent_loop, run_agent_loop_nested, test_ctx,
        };
        use crate::task_kind::TaskKind;
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
        use std::collections::HashMap;
        use std::sync::Arc;
        use std::time::Duration;

        fn task_ctx(
            dir: std::path::PathBuf,
            root_raw_stream_path: Option<std::path::PathBuf>,
        ) -> AgentLoopRuntime {
            AgentLoopRuntime {
                cwd: Some(dir.clone()),
                extra_env: None,
                base_env: None,
                sandbox: tools::SandboxMode::Roots(vec![dir]),
                audit_log: None,
                allowed_tools: None,
                task_fn: None,
                audit_lock: None,
                id_prefix: String::new(),
                message_prefix: String::new(),
                discovered_skills: None,
                path_context: crate::clients::path_context::PathContext::default(),
                root_raw_stream_path,
            }
        }

        fn lines(path: &std::path::Path) -> Vec<serde_json::Value> {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        /// Build a model that produces a single Done turn with the given
        /// summary, so the nested loop completes cleanly in one turn.
        fn done_model(summary: &str) -> MockCompletionModel {
            MockCompletionModel::from_stream_turns([vec![
                MockStreamEvent::text(summary),
                MockStreamEvent::tool_call(
                    "done1",
                    "Done",
                    serde_json::json!({"summary": summary}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ]])
        }

        /// Build a model that produces a Read tool call (non-Done) in one
        /// turn so the nested loop exercises the code path that writes
        /// tool_use + tool_result events. Done is produced in a second turn.
        fn read_then_done_model(path: &str, summary: &str) -> MockCompletionModel {
            let path_owned = path.to_string();
            let summary_owned = summary.to_string();
            MockCompletionModel::from_stream_turns([
                vec![
                    MockStreamEvent::tool_call(
                        "c1",
                        "Read",
                        serde_json::json!({"file_path": path_owned}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text(summary_owned.clone()),
                    MockStreamEvent::tool_call(
                        "done1",
                        "Done",
                        serde_json::json!({"summary": summary_owned}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ])
        }

        /// Two concurrent sibling tasks each get their own JSONL file.  Each
        /// sibling file is valid JSONL containing only that loop's events.
        /// The parent's raw stream (pre-created with sentinel content) is
        /// untouched — it does not receive children's internal events.
        #[tokio::test]
        async fn concurrent_siblings_each_get_own_file() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("stream-implement.jsonl");
            // Parent stream starts with sentinel content.
            std::fs::write(&root, "[]\n").unwrap();

            let ctx = task_ctx(dir.path().to_path_buf(), Some(root.clone()));
            let cancel = crate::clients::cancel::CancelToken::new();

            // Pre-create the expected child filename with sentinel content.
            // The nested loop should detect it exists and fall back to a
            // suffixed name (exclusive creation).
            let expected_child = dir.path().join("stream-implement.task.a3f1.jsonl");
            std::fs::write(&expected_child, "pre-existing sentinel\n").unwrap();

            // Two models with distinguishable summaries so sibling files differ.
            let model_a = read_then_done_model("a.txt", "alpha");
            let model_b = read_then_done_model("b.txt", "beta");

            // Run two siblings concurrently via join! rather than spawn so
            // stack-local borrows of ctx and cancel satisfy the lifetime.
            let fut_a = run_agent_loop_nested(
                model_a.erase(),
                "sibling A",
                Some("system".to_string()),
                &ctx,
                &cancel,
                None,
                "[sib-a]",
                "a3f1",
                5.0,
                10,
                0,
                None,
                None,
                None,
                false,
                Box::new(ConversationStub::new("")),
                "p1",
                "Task Scout a3f1",
                "Scout",
            );
            let fut_b = run_agent_loop_nested(
                model_b.erase(),
                "sibling B",
                Some("system".to_string()),
                &ctx,
                &cancel,
                None,
                "[sib-b]",
                "b72e",
                5.0,
                10,
                0,
                None,
                None,
                None,
                false,
                Box::new(ConversationStub::new("")),
                "p2",
                "Task Scout b72e",
                "Scout",
            );

            let (r_a, r_b) = tokio::join!(fut_a, fut_b);
            assert!(r_a.is_ok());
            assert!(r_b.is_ok());

            // The pre-created file must still contain the sentinel.
            assert_eq!(
                std::fs::read_to_string(&expected_child).unwrap().trim(),
                "pre-existing sentinel",
                "pre-existing child file must be untouched"
            );

            // A suffixed file should have been created for sibling A.
            let suffixed = dir.path().join("stream-implement.task.a3f1-0001.jsonl");
            assert!(
                suffixed.exists(),
                "suffixed file {} should exist",
                suffixed.display()
            );

            // Sibling B file with distinct chain.
            let second_child = dir.path().join("stream-implement.task.b72e.jsonl");
            assert!(
                second_child.exists(),
                "second sibling file {} should exist",
                second_child.display()
            );

            // Each sibling file must be valid JSONL with its own events.
            let first_events = lines(&suffixed);
            assert!(
                !first_events.is_empty(),
                "first sibling file must contain events"
            );
            // Each file must contain the assistant text event for its sibling.
            let first_has_text = first_events.iter().any(|ev| {
                ev["type"] == "assistant"
                    && ev["message"]["content"].as_array().is_some_and(|c| {
                        c.iter()
                            .any(|ci| ci.get("text").is_some_and(|t| t == "alpha"))
                    })
            });
            assert!(first_has_text, "first sibling must contain 'alpha' text");

            let second_events = lines(&second_child);
            assert!(
                !second_events.is_empty(),
                "second sibling file must contain events"
            );
            let second_has_text = second_events.iter().any(|ev| {
                ev["type"] == "assistant"
                    && ev["message"]["content"].as_array().is_some_and(|c| {
                        c.iter()
                            .any(|ci| ci.get("text").is_some_and(|t| t == "beta"))
                    })
            });
            assert!(second_has_text, "second sibling must contain 'beta' text");

            // Each sibling's tool_use event must have a matching tool_result.
            for (events, label) in [(&first_events, "first"), (&second_events, "second")] {
                let calls: std::collections::HashSet<String> = events
                    .iter()
                    .filter(|ev| {
                        ev["message"]["content"].as_array().is_some_and(|c| {
                            c.iter()
                                .any(|ci| ci.get("type").is_some_and(|t| t == "tool_use"))
                        })
                    })
                    .map(|ev| {
                        ev["message"]["content"][0]["id"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect();
                let results: std::collections::HashSet<String> = events
                    .iter()
                    .filter(|ev| {
                        ev["message"]["content"].as_array().is_some_and(|c| {
                            c.iter()
                                .any(|ci| ci.get("type").is_some_and(|t| t == "tool_result"))
                        })
                    })
                    .map(|ev| {
                        ev["message"]["content"][0]["tool_use_id"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect();
                assert!(!calls.is_empty(), "{label} sibling must have tool calls");
                assert!(
                    !results.is_empty(),
                    "{label} sibling must have tool results"
                );
                for call_id in &calls {
                    assert!(
                        results.contains(call_id),
                        "{label} sibling: result for call {call_id} must exist"
                    );
                }
            }

            // Parent file must not include children's internal events.
            let parent_lines = std::fs::read_to_string(&root).unwrap();
            assert_eq!(
                parent_lines.trim(),
                "[]",
                "parent raw stream must only have sentinel content, not child events"
            );
        }

        /// A grandchild (depth-2 task) creates a three-segment filename.
        #[tokio::test]
        async fn grandchild_gets_three_segment_filename() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("stream-main.jsonl");
            std::fs::write(&root, "[]\n").unwrap();

            // The grandchild model will Read this file, so it must exist.
            std::fs::write(dir.path().join("gc.txt"), "hello from grandchild\n").unwrap();

            // Build a runner that will be injected as the child's task_fn.
            // The child's model produces a Task call (to spawn a grandchild).
            // But we need the child to actually have a task_fn that can be
            // invoked.  We set up the child_ctx with task_fn pointing to a
            // runner that, when invoked, returns a static result.
            let grandchild_model = read_then_done_model("gc.txt", "gc-done").erase();
            let child_id_chain = "0001".to_string();

            // Run as if a parent spawned a child at id_chain "0001".
            let ctx = task_ctx(dir.path().to_path_buf(), Some(root.clone()));
            let cancel = crate::clients::cancel::CancelToken::new();

            // The parent model calls Task, which invokes the task_fn.  We
            // pre-seed the ctx's task_fn so the parent's Task tool actually
            // spawns a child (depth 1), and *that* child's task_fn spawns
            // the grandchild (depth 2).
            let mut ctx_with_fn = ctx.clone();
            ctx_with_fn.task_fn = Some(make_task_runner_at_depth(
                grandchild_model.clone(),
                None,
                None,
                cancel.clone(),
                ctx.clone(),
                "[task]".to_string(),
                "child-mdl".to_string(),
                5.0,
                10,
                0, // depth 0 for the runner that *becomes* depth 1 when called
                child_id_chain.clone(),
                0,
                None,
                None,
                None,
                false,
            ));

            // Now the child model calls Task to spawn a grandchild — the
            // injected task_fn will route this to itself at depth 1→2.
            // The child_model must call Task, and we run as a nested loop
            // at id_chain "0001".
            let child_model = MockCompletionModel::from_stream_turns([
                vec![
                    MockStreamEvent::tool_call(
                        "tc1",
                        "Task",
                        serde_json::json!({"kind": "Scout", "prompt": "spawn"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("child done"),
                    MockStreamEvent::tool_call(
                        "dc1",
                        "Done",
                        serde_json::json!({"summary": "child done"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ])
            .erase();

            let _ = run_agent_loop_nested(
                child_model,
                "child",
                Some("system".to_string()),
                &ctx_with_fn,
                &cancel,
                None,
                "[task]",
                "0001",
                5.0,
                10,
                0,
                None,
                None,
                None,
                false,
                Box::new(ConversationStub::new("")),
                "p1",
                "Task Scout 0001",
                "Scout",
            )
            .await
            .expect("child nested loop must succeed");

            // The child (depth 1) file must exist.
            let child_file = dir.path().join("stream-main.task.0001.jsonl");
            assert!(
                child_file.exists(),
                "child file {} should exist",
                child_file.display()
            );

            // The grandchild (depth 2) file must exist with the full chain:
            // stream-main.task.<child_chain>.<gc_seg>.jsonl
            // The gc_seg is a four-hex-digit segment appended by the task runner
            // to the parent chain, so we get stream-main.task.0001.NNNN.jsonl.
            let gc_files: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with("stream-main.task.0001.")
                        && name.ends_with(".jsonl")
                        && name != "stream-main.task.0001.jsonl"
                })
                .collect();
            assert!(
                !gc_files.is_empty(),
                "grandchild file must exist (expected stream-main.task.0001.NNNN.jsonl)"
            );

            // Verify the exact three-segment chain pattern (four hex digits).
            let gc_name = gc_files[0].file_name().to_string_lossy().to_string();
            assert!(
                gc_name.starts_with("stream-main.task.0001.")
                    && gc_name.ends_with(".jsonl")
                    && gc_name != "stream-main.task.0001.jsonl",
                "grandchild file name must have three hex-segment chain: got {gc_name}"
            );
            // The third segment must be exactly four hex digits + .jsonl.
            let seg_part = gc_name
                .strip_prefix("stream-main.task.0001.")
                .unwrap()
                .strip_suffix(".jsonl")
                .unwrap();
            assert_eq!(
                seg_part.len(),
                4,
                "third segment must be four hex digits, got {seg_part}"
            );
            assert!(
                seg_part.chars().all(|c| c.is_ascii_hexdigit()),
                "third segment must be hex, got {seg_part}"
            );

            // The grandchild file must be valid JSONL.
            let gc_events = lines(&gc_files[0].path());
            assert!(!gc_events.is_empty(), "grandchild file must contain events");
            // Must have tool call + result pair.
            let has_call = gc_events.iter().any(|ev| {
                ev["message"]["content"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|ci| ci.get("type").is_some_and(|t| t == "tool_use"))
                })
            });
            assert!(has_call, "grandchild file must contain a tool_use event");
        }

        /// With no root stream path, no per-task files are created.
        #[tokio::test]
        async fn no_root_stream_creates_no_sibling_files() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = task_ctx(dir.path().to_path_buf(), None);

            let model = done_model("done").erase();

            let result = run_agent_loop_nested(
                model,
                "task",
                Some("system".to_string()),
                &ctx,
                &crate::clients::cancel::CancelToken::new(),
                None,
                "[task]",
                "0001",
                5.0,
                10,
                0,
                None,
                None,
                None,
                false,
                Box::new(ConversationStub::new("")),
                "p1",
                "c1",
                "Scout",
            )
            .await;
            result.expect("nested loop with no root stream must succeed");

            // No task-*.jsonl files must exist.
            let task_files: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains("task."))
                .collect();
            assert!(
                task_files.is_empty(),
                "no task files should be created when root_raw_stream_path is None, got {:?}",
                task_files.iter().map(|e| e.file_name()).collect::<Vec<_>>()
            );
        }

        /// When the root stream is disabled (None), child tool results must
        /// still reach the live event consumer exactly once.
        #[tokio::test]
        async fn no_root_stream_child_tool_results_reach_live_events_once() {
            gremlins_magic_bus::init_bus();
            let _env = crate::test_support::EnvGuard::lock();
            let dir = tempfile::tempdir().unwrap();
            let run_id = "raw_stream_live_event_test";

            let mut ctx = task_ctx(dir.path().to_path_buf(), None);
            ctx.message_prefix = run_id.to_string();

            let collected = Arc::new(std::sync::Mutex::new(
                Vec::<gremlins_magic_bus::EventKind>::new(),
            ));
            let sink = collected.clone();
            let (_sub, mut bus_rx) = gremlins_magic_bus::subscribe_run(run_id);
            let drain = tokio::spawn(async move {
                loop {
                    match bus_rx.recv().await {
                        gremlins_magic_bus::Recv::Event(e) => {
                            sink.lock().unwrap().push(e.into_kind());
                        }
                        gremlins_magic_bus::Recv::Closed => break,
                        gremlins_magic_bus::Recv::Lagged(_) => continue,
                    }
                }
            });

            // Use a model that makes a Read call, so we get ToolResult events.
            let model = read_then_done_model("a.txt", "finished").erase();

            let result = run_agent_loop_nested(
                model,
                "task",
                Some("system".to_string()),
                &ctx,
                &crate::clients::cancel::CancelToken::new(),
                None,
                "[task]",
                "0001",
                5.0,
                10,
                0,
                None,
                None,
                None,
                false,
                Box::new(ConversationStub::new("")),
                "p1",
                "c1",
                "Scout",
            )
            .await;
            result.expect("nested loop must succeed");

            let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;

            let events = collected.lock().unwrap().clone();

            // There must be at least one ToolResult for the Read call.
            let tool_results: Vec<_> = events
                .iter()
                .filter(|e| matches!(e, gremlins_magic_bus::EventKind::ToolResult { .. }))
                .collect();
            assert!(
                !tool_results.is_empty(),
                "child tool results must be delivered via live events even without root stream"
            );

            // Each tool result id must appear exactly once.
            let mut id_counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for e in &events {
                if let gremlins_magic_bus::EventKind::ToolResult { id, .. } = e {
                    *id_counts.entry(id.clone()).or_insert(0) += 1;
                }
            }
            for (id, count) in &id_counts {
                assert_eq!(
                    *count, 1,
                    "ToolResult id {id} must appear exactly once in live events, got {count}"
                );
            }
        }

        /// Integration test: run_agent_loop with raw_path set. The parent
        /// model calls Task, which delegates to a nested loop. The child's
        /// internal events go ONLY to the per-task JSONL file, never to the
        /// parent's raw stream. The parent's stream receives its own events
        /// (including the ToolResult from the task) but not child-internal
        /// assistant/tool_use/tool_result events.
        #[tokio::test]
        async fn outer_loop_raw_path_isolates_child_transcript() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("stream-main.jsonl");
            std::fs::write(&root, "[]\n").unwrap();
            std::fs::write(dir.path().join("x.txt"), "hello\n").unwrap();

            let task_call = |id: &str| {
                MockStreamEvent::tool_call(
                    id,
                    "Task",
                    serde_json::json!({"kind": "Scout", "prompt": "scout this"}),
                )
            };
            let done_call = |id: &str, summary: &str| {
                MockStreamEvent::tool_call(id, "Done", serde_json::json!({"summary": summary}))
            };

            // Parent: turn 0 calls Task → turn 1 calls Done.
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
            // Child: two turns — first calls Read (non-Done, tool_use written),
            // then text + Done. The Done tool_use is handled before
            // execute_tools and won't appear in the raw stream.
            let child = MockCompletionModel::from_stream_turns([
                vec![
                    MockStreamEvent::tool_call(
                        "tc_child",
                        "Read",
                        serde_json::json!({"file_path": "x.txt"}),
                    ),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
                vec![
                    MockStreamEvent::text("inner work"),
                    done_call("d2", "inner done"),
                    MockStreamEvent::final_response_with_default_usage(),
                ],
            ]);
            let child_handle = child.clone();

            // Wire child into a TaskModelSelector so the Task tool dispatches
            // to it.  Without a selector, the parent model handles the task.
            let factory: TaskModelFactory =
                crate::clients::agent::task_models::sync_factory(move |_spec: &str| {
                    Ok(TaskModel {
                        model: child_handle.clone().erase(),
                        temperature: None,
                        max_tokens: None,
                        extra: None,
                        preserve_reasoning: None,
                    })
                });
            let selector = TaskModelSelector::new(
                HashMap::from([(TaskKind::Scout, "openai/mini".to_string())]),
                factory,
            )
            .expect("configured");

            let mut ctx = test_ctx(Some(dir.path().to_path_buf()), Some(root.clone()));
            ctx.idle_timeout = 5.0;
            ctx.params.idle_timeout = Some(5.0);
            ctx.params.task_models = Some(selector);
            let cancel = crate::clients::cancel::CancelToken::new();

            run_agent_loop(
                parent.clone().erase(),
                "go",
                ctx,
                cancel,
                loop_opts(None),
                crate::clients::interactive::InteractiveSession::unattached(),
            )
            .await
            .expect("outer loop must succeed");

            // ── Parent stream: must NOT contain child-internal events ──
            let parent_lines = std::fs::read_to_string(&root).unwrap();
            let parent_events: Vec<_> = parent_lines
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .collect();

            // The parent's raw stream must have at least the outer-loop
            // assistant events (tool_use for Task, assistant for Done).
            let parent_has_task_call = parent_events.iter().any(|ev| {
                ev["message"]["content"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|ci| ci.get("name").is_some_and(|n| n == "Task"))
                })
            });
            assert!(
                parent_has_task_call,
                "parent raw stream must contain its own Tool call to Task"
            );

            // The parent must NOT contain child-internal events. The child
            // model emits "inner work" text — that must NOT appear.
            let parent_has_child_text = parent_events.iter().any(|ev| {
                ev["message"]["content"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|ci| ci.get("text").is_some_and(|t| t == "inner work"))
                })
            });
            assert!(
                !parent_has_child_text,
                "parent raw stream must NOT contain child-internal assistant text"
            );

            // ── Per-task file: must exist and contain the child's events ──
            // The child's chain segment is assigned by the task runner, so
            // we look for files matching stream-main.task.*.jsonl (excluding
            // the parent's stream-main.jsonl).
            let task_files: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with("stream-main.task.") && name.ends_with(".jsonl")
                })
                .collect();
            assert!(
                !task_files.is_empty(),
                "per-task JSONL file must exist when outer loop has raw_path set"
            );

            let child_events = lines(&task_files[0].path());
            let child_has_text = child_events.iter().any(|ev| {
                ev["message"]["content"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|ci| ci.get("text").is_some_and(|t| t == "inner work"))
                })
            });
            assert!(
                child_has_text,
                "per-task file must contain the child's own assistant text"
            );

            // The child's tool_call (tc_child -> Read) must also be present.
            // (Done is intercepted by handle_done_decision and never written
            //  to raw — a known characteristic, not a bug.)
            let child_has_read_call = child_events.iter().any(|ev| {
                ev["message"]["content"].as_array().is_some_and(|c| {
                    c.iter().any(|ci| {
                        ci.get("name").is_some_and(|n| n == "Read")
                            && ci.get("id").is_some_and(|i| i == "tc_child")
                    })
                })
            });
            assert!(
                child_has_read_call,
                "per-task file must contain the child's own Read tool call"
            );

            // ── Captured events: parent must include ToolResult from child ──
            // (The parent gets a Task result string; the child's internal
            //  tool_use/tool_result for Done are not captured by the parent.)
            // We already verified parent raw stream isolation above.
        }
    }
}
