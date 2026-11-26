//! Scripted headless delegation through the real Task dispatcher and durable collector.

#[tokio::test(start_paused = true)]
async fn retry_persists_failed_attempt_and_success_once() {
    retry_persists_attempts(false).await;
}

#[tokio::test(start_paused = true)]
async fn retry_persists_reported_failed_attempt_and_success_once() {
    retry_persists_attempts(true).await;
}

async fn retry_persists_attempts(reported: bool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state.json"), "{}").unwrap();
    let run = uuid::Uuid::new_v4().to_string();
    let collector = AccountingCollector::start(
        &run,
        Arc::new(FileSystemStateStore::from_path(dir.path().to_owned())),
    )
    .unwrap();
    collector
        .register_execution("retry-execution", "plan")
        .unwrap();
    let mut failure = vec![Event::text("discard me")];
    if reported {
        failure.push(usage(40));
    }
    failure.push(Event::error("http://429: slow down"));
    let model = named_mock("retry-model", [failure, done(100)]);
    let probe = model.clone();
    let model = rig_core::driver::Model::new(
        ReportedFailureWire(model.wire),
        RetryTransport(model.transport),
    )
    .erase();
    let model_id = model.id().unwrap().to_owned();
    let mut ctx = test_ctx(Some(dir.path().to_owned()), None);
    ctx.idle_timeout = 30.0;
    ctx.params.gremlin_id = Some(run.clone());
    ctx.params.output_corr_id = Some(
        CorrId::new()
            .with_run(&run)
            .with_stage("plan")
            .with_stage_run_id("retry-execution"),
    );
    ctx.prepopulated_conversation = crate::conversation::new_blank_conversation("hi", false, None);
    let result = run_agent_loop(
        model,
        "finish",
        ctx,
        CancelToken::new(),
        loop_opts(None),
        InteractiveSession::unattached(),
    )
    .await
    .unwrap();
    assert_eq!(probe.requests().len(), 2);
    assert_eq!(result.token_usage.unwrap().turns, 1);
    collector.flush().await.unwrap();
    let rows = records(dir.path());
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].usage.request_id, rows[1].usage.request_id);
    assert!(rows.iter().all(|r| r.usage.model == model_id
        && r.usage.agent_id.is_empty()
        && r.stage_path == "plan"
        && r.corr_id.stage_run_id() == Some("retry-execution")));
    assert_eq!(rows[0].corr_id.turn(), rows[1].corr_id.turn());
    let failed = rows.iter().find(|r| !r.usage.completed).unwrap();
    assert_eq!(failed.usage.reported, reported);
    assert_eq!(failed.usage.usage.turns, 0);
    assert_eq!(failed.usage.usage.prompt_tokens, reported.then_some(40));
    assert_eq!(failed.usage.usage.completion_tokens, reported.then_some(4));
    assert_eq!(
        failed.usage.usage.cached_input_tokens,
        reported.then_some(20)
    );
    let success = rows.iter().find(|r| r.usage.completed).unwrap();
    assert!(success.usage.reported);
    assert_eq!(success.usage.usage.turns, 1);
    assert_eq!(success.usage.usage.prompt_tokens, Some(100));
    assert_eq!(success.usage.usage.completion_tokens, Some(10));
    assert_eq!(success.usage.usage.cached_input_tokens, Some(50));
    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(
        state["token_usage"]["prompt_tokens"],
        if reported { 140 } else { 100 }
    );
    assert_eq!(
        state["token_usage"]["completion_tokens"],
        if reported { 14 } else { 10 }
    );
    assert_eq!(
        state["token_usage"]["cached_input_tokens"],
        if reported { 70 } else { 50 }
    );
    assert_eq!(state["token_usage"]["turns"], 1);
    collector.flush().await.unwrap();
    assert_eq!(records(dir.path()).len(), 2);
}
#[tokio::test(start_paused = true)]
async fn retry_live_usage_charges_failed_attempt_without_replacing_last_prompt() {
    let run = uuid::Uuid::new_v4().to_string();
    let (_guard, mut events) = gremlins_magic_bus::subscribe_run(&run);
    let mock = named_mock("retry-live", [vec![usage(10)], done(20)]);
    let probe = mock.clone();
    let model = rig_core::driver::Model::new(
        ReportedFailureWire(mock.wire),
        RetryTransport(mock.transport),
    )
    .erase();
    let mut ctx = test_ctx(None, None);
    ctx.params.gremlin_id = Some(run);
    let result = run_agent_loop(
        model,
        "finish",
        ctx,
        CancelToken::new(),
        loop_opts(None),
        InteractiveSession::unattached(),
    )
    .await
    .unwrap();
    assert_eq!(probe.requests().len(), 2);
    let local = result.token_usage.unwrap();
    assert_eq!(local.prompt_tokens, Some(30));
    assert_eq!(local.turns, 1);
    let mut live = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventKind::TokenUsage {
            usage,
            last_prompt_tokens,
            ..
        } = event.kind()
        {
            live.push((usage.clone(), *last_prompt_tokens));
        }
    }
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].0.prompt_tokens, Some(30));
    assert_eq!(live[0].0.completion_tokens, Some(3));
    assert_eq!(live[0].0.cached_input_tokens, Some(15));
    assert_eq!(live[0].0.turns, 1);
    assert_eq!(live[0].1, Some(20));
}

use super::*;
use crate::clients::agent::task_models::{sync_factory, TaskModel, TaskModelSelector};
use crate::executor::accounting::{AccountingCollector, UsageRecord};
use crate::executor::state::FileSystemStateStore;
use crate::task_kind::TaskKind;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent as Event};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

// Mock final_response ends a successful stream. End with usage and fail in the
// same decoder step instead, modelling a provider failure with billed counters.
#[derive(Clone)]
struct ReportedFailureWire(rig_core::test_utils::MockScript);
impl rig_core::wire::Wire for ReportedFailureWire {
    type Op = Completion;
    type Payload = rig_core::completion::CompletionRequest;
    type Frame = rig_core::test_utils::MockFrame;
    type Event = rig_core::test_utils::MockFrame;
    type Reassembler = rig_core::test_utils::MockDocument;
    type Decoder<'id> = ReportedFailureDecoder<'id>;
    fn describe(&self) -> rig_core::wire::Descriptor<'_> {
        self.0.describe()
    }
    fn encode(
        &self,
        request: rig_core::wire::Request<Self>,
        mode: rig_core::wire::Mode,
    ) -> Result<Self::Payload, rig_core::error::EncodeError> {
        self.0.encode(request, mode)
    }
    fn decoder<'id>(&self) -> Self::Decoder<'id> {
        ReportedFailureDecoder(self.0.decoder())
    }
}
struct ReportedFailureDecoder<'id>(rig_core::test_utils::MockDecoder<'id>);
impl<'id> rig_core::wire::Decoder<'id, Completion, rig_core::test_utils::MockFrame>
    for ReportedFailureDecoder<'id>
{
    type Event = rig_core::test_utils::MockFrame;
    fn classify(&self, frame: Self::Event) -> rig_core::wire::WireEvent<Self::Event> {
        self.0.classify(frame)
    }
    fn decode(
        &mut self,
        event: Self::Event,
        out: rig_core::wire::Out<'id, Completion>,
    ) -> Result<rig_core::wire::Flow, rig_core::error::ProviderError> {
        if let rig_core::test_utils::MockFrame::Event(Event::FinalResponse(ref finish)) = event {
            if matches!(finish.usage.input_tokens, Some(10 | 40)) {
                let _ = out.end(finish.clone());
                return Err(rig_core::error::ProviderError::Truncated);
            }
        }
        self.0.decode(event, out)
    }
}
#[derive(Clone)]
struct RetryTransport(rig_core::test_utils::MockRuntime);
impl rig_core::driver::Transport<ReportedFailureWire> for RetryTransport {
    fn send(
        &self,
        request: rig_core::completion::CompletionRequest,
        exchange: rig_core::driver::Exchange,
    ) -> rig_core::driver::Opening<rig_core::test_utils::MockFrame> {
        rig_core::driver::Transport::<rig_core::test_utils::MockScript>::send(
            &self.0, request, exchange,
        )
    }
}

fn named_mock(id: &str, turns: impl IntoIterator<Item = Vec<Event>>) -> MockCompletionModel {
    let mut model = MockCompletionModel::from_stream_turns(turns);
    model.wire = model.wire.with_id(id);
    model
}

fn usage(tokens: u64) -> Event {
    Event::final_response(
        Usage::new()
            .input_tokens(tokens)
            .output_tokens(tokens / 10)
            .cached_input_tokens(tokens / 2),
    )
}

fn done(tokens: u64) -> Vec<Event> {
    vec![
        Event::tool_call("done", "Done", json!({"summary":"finished"})),
        usage(tokens),
    ]
}

fn task(id: &str, kind: &str) -> Event {
    Event::tool_call(
        id,
        "Task",
        json!({"kind":kind,"prompt":"finish delegated work"}),
    )
}

fn selected(model: DynModel<Completion>) -> TaskModel {
    TaskModel {
        model,
        backend: None,
        temperature: None,
        max_tokens: None,
        extra: None,
        preserve_reasoning: None,
    }
}

fn records(dir: &std::path::Path) -> Vec<UsageRecord> {
    // The append log is authoritative while production is still live; the
    // compatible JSON checkpoint is materialized at flush barriers.
    std::fs::read_to_string(dir.join("native_accounting.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn headless_parallel_and_nested_tasks_persist_each_request_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state.json"), "{}").unwrap();
    let run = uuid::Uuid::new_v4().to_string();
    let collector = AccountingCollector::start(
        &run,
        Arc::new(FileSystemStateStore::from_path(dir.path().to_owned())),
    )
    .unwrap();
    collector.register_execution("execution", "plan").unwrap();
    let (_events_guard, mut events) = gremlins_magic_bus::subscribe_run(&run);
    let parent = named_mock(
        "parent-model",
        [
            vec![
                task("scout", "Scout"),
                task("implement", "Implement"),
                usage(50_000),
            ],
            vec![
                Event::tool_call("done", "Done", json!({"summary":"finished"})),
                Event::final_response(
                    Usage::new()
                        .input_tokens(60_000)
                        .output_tokens(6_000)
                        .cached_input_tokens(40_000),
                ),
            ],
        ],
    );
    // Bound child loops to one turn: the delegating child exits after its Task.
    // Done cannot share a turn with Task; that would reject the entire batch.
    let scout = named_mock("scout-model", [vec![task("nested", "Scout"), usage(30)]]).erase();
    let implement = named_mock("implement-model", [done(20)]).erase();
    let nested = named_mock("nested-model", [done(10)]).erase();
    let expected_models = [
        scout.id().unwrap().to_owned(),
        implement.id().unwrap().to_owned(),
        nested.id().unwrap().to_owned(),
    ];
    let scouts = std::sync::atomic::AtomicUsize::new(0);
    let selector = TaskModelSelector::new(
        HashMap::from([
            (TaskKind::Scout, "scout".into()),
            (TaskKind::Implement, "implement".into()),
        ]),
        sync_factory(move |spec| {
            Ok(selected(if spec == "scout" {
                if scouts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    scout.clone()
                } else {
                    nested.clone()
                }
            } else {
                implement.clone()
            }))
        }),
    );
    let parent_probe = parent.clone();
    let model = parent.erase();
    let parent_model = model.id().unwrap().to_owned();
    assert_eq!(parent_model, "parent-model");
    assert_eq!(
        expected_models,
        ["scout-model", "implement-model", "nested-model"]
    );
    let cancel = CancelToken::new();
    let mut runtime = test_tool_ctx();
    runtime.cwd = Some(dir.path().to_owned());
    runtime.message_prefix = run.clone();
    runtime.corr_id = CorrId::new()
        .with_run(&run)
        .with_stage("plan")
        .with_stage_run_id("execution");
    runtime.task_fn = Some(super::super::task::make_task_runner(
        model.clone(),
        selector,
        None,
        cancel.clone(),
        runtime.clone(),
        "[plan]".into(),
        "parent".into(),
        5.0,
        1,
        0,
        None,
        None,
        None,
        false,
    ));
    let mut raw = None;
    let mut captured = None;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_agent_loop_core(
            false,
            None,
            &model,
            "delegate",
            None,
            &runtime,
            &tools::tool_definitions(None, false, ToolMode::Full),
            &cancel,
            &loop_opts(None),
            "[plan]",
            2,
            5.0,
            &mut raw,
            &mut captured,
            false,
            None,
            0,
            0,
            InteractiveSession::unattached(),
            crate::conversation::new_blank_conversation("hi", false, None),
            true,
            "",
            "",
            "",
            "",
        ),
    )
    .await
    .expect("scripted headless loop must terminate")
    .unwrap();
    collector.flush().await.unwrap();
    let records = records(dir.path());
    assert_eq!(records.len(), 5);
    assert_eq!(
        records
            .iter()
            .map(|r| &r.usage.request_id)
            .collect::<HashSet<_>>()
            .len(),
        5
    );
    assert_eq!(
        records
            .iter()
            .filter_map(|r| r.usage.usage.prompt_tokens)
            .sum::<u64>(),
        110_060
    );
    assert!(records
        .iter()
        .all(|r| r.usage.completed && r.usage.reported && r.stage_path == "plan"));
    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(state["token_usage"]["prompt_tokens"], 110_060);
    assert_eq!(state["token_usage"]["completion_tokens"], 11_006);
    assert_eq!(state["token_usage"]["cached_input_tokens"], 65_030);
    assert_eq!(state["token_usage"]["turns"], 5);
    let summaries: Vec<Value> = std::fs::read_to_string(dir.path().join("token_usage.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["stage_run_id"], "execution");
    for totals in [&state["token_usage"], &summaries[0]] {
        assert_eq!(totals["prompt_tokens"], 110_060);
        assert_eq!(totals["completion_tokens"], 11_006);
        assert_eq!(totals["cached_input_tokens"], 65_030);
        assert_eq!(totals["turns"], 5);
    }
    let mut live = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventKind::TokenUsage {
            usage,
            last_prompt_tokens,
            last_cached_input_tokens,
            ..
        } = event.kind()
        {
            live.push((
                usage.clone(),
                *last_prompt_tokens,
                *last_cached_input_tokens,
            ));
        }
    }
    assert_eq!(live.len(), 2);
    let (usage, last_prompt, last_cached) = live.last().unwrap();
    assert_eq!(usage.prompt_tokens, Some(110_000));
    assert_eq!(*last_prompt, Some(60_000));
    assert_eq!(*last_cached, Some(40_000));
    for row in &records {
        let tokens = row.usage.usage.prompt_tokens.unwrap();
        assert_eq!(row.usage.usage.completion_tokens, Some(tokens / 10));
        assert_eq!(
            row.usage.usage.cached_input_tokens,
            Some(if tokens == 60_000 { 40_000 } else { tokens / 2 })
        );
        assert_eq!(row.corr_id.stage_run_id(), Some("execution"));
    }
    assert_eq!(result.token_usage.unwrap().prompt_tokens, Some(110_000));
    let mut parents: Vec<_> = records
        .iter()
        .filter(|r| r.usage.agent_id.is_empty())
        .collect();
    assert_eq!(parents.len(), 2);
    assert_eq!(parent_probe.requests().len(), 2);
    assert!(parents.iter().all(|r| r.usage.model == parent_model));
    parents.sort_by_key(|r| r.corr_id.turn());
    assert_eq!(
        parents.last().unwrap().usage.usage.prompt_tokens,
        Some(60_000)
    );
    let children: Vec<_> = records
        .iter()
        .filter(|r| !r.usage.agent_id.is_empty())
        .collect();
    assert_eq!(children.len(), 3);
    for (tokens, model) in [30, 20, 10].into_iter().zip(expected_models) {
        let child = children
            .iter()
            .find(|r| r.usage.usage.prompt_tokens == Some(tokens))
            .unwrap();
        assert_eq!(child.usage.model, model);
        assert_eq!(child.corr_id.stage_run_id(), Some("execution"));
    }
    assert_eq!(
        children
            .iter()
            .map(|r| &r.usage.agent_id)
            .collect::<HashSet<_>>()
            .len(),
        3
    );
}

// Complete one turn, then hold the next request open. Used both for a parent
// whose child returned and for a child whose enclosing Task is still outstanding.
#[derive(Clone)]
struct HoldParent {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    waiting: Arc<tokio::sync::Notify>,
    first_turn: Vec<Event>,
}
impl rig_core::driver::Transport<rig_core::test_utils::MockScript> for HoldParent {
    fn send(
        &self,
        _: rig_core::completion::CompletionRequest,
        _: rig_core::driver::Exchange,
    ) -> rig_core::driver::Opening<rig_core::test_utils::MockFrame> {
        use futures::StreamExt;
        use rig_core::driver::{Opened, Opening};
        use rig_core::test_utils::MockFrame;
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Opening::ready(Opened::new(
                futures::stream::iter(self.first_turn.clone())
                    .map(MockFrame::Event)
                    .map(Ok),
            ))
        } else {
            let waiting = self.waiting.clone();
            Opening::new(async move {
                waiting.notify_one();
                std::future::pending().await
            })
        }
    }
}

#[tokio::test]
async fn cancellation_preserves_child_usage_before_parent_returns() {
    cancellation_preserves_usage(false).await;
}

#[tokio::test]
async fn cancellation_while_task_is_outstanding_preserves_child_usage() {
    cancellation_preserves_usage(true).await;
}

async fn wait_for_projection(dir: &std::path::Path, count: usize, turns: u64) {
    // WAL appends precede projections; observe both without a flush barrier.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state = std::fs::read(dir.join("state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            let rows = std::fs::read_to_string(dir.join("token_usage.jsonl"))
                .ok()
                .and_then(|text| {
                    text.lines()
                        .map(serde_json::from_str::<Value>)
                        .collect::<Result<Vec<_>, _>>()
                        .ok()
                });
            if records(dir).len() == count
                && state
                    .as_ref()
                    .and_then(|s| s["token_usage"]["turns"].as_u64())
                    == Some(turns)
                && rows
                    .as_ref()
                    .is_some_and(|rows| rows.len() == 1 && rows[0]["turns"].as_u64() == Some(turns))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("live accounting must persist while the model/Task is blocked");
}

fn assert_live_totals(dir: &std::path::Path) {
    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    let rows: Vec<Value> = std::fs::read_to_string(dir.join("token_usage.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["stage"], "plan");
    assert_eq!(rows[0]["stage_run_id"], "cancel-execution");
    for totals in [&state["token_usage"], &rows[0]] {
        assert_eq!(totals["prompt_tokens"], 50_030);
        assert_eq!(totals["completion_tokens"], 5_003);
        assert_eq!(totals["cached_input_tokens"], 25_015);
        assert_eq!(totals["turns"], 2);
    }
}

async fn cancellation_preserves_usage(task_outstanding: bool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state.json"), "{}").unwrap();
    let run = uuid::Uuid::new_v4().to_string();
    let collector = AccountingCollector::start(
        &run,
        Arc::new(FileSystemStateStore::from_path(dir.path().to_owned())),
    )
    .unwrap();
    collector
        .register_execution("cancel-execution", "plan")
        .unwrap();
    let waiting = Arc::new(tokio::sync::Notify::new());
    let parent_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model = rig_core::driver::Model::new(
        rig_core::test_utils::MockScript::default().with_id("cancel-parent"),
        HoldParent {
            calls: parent_calls.clone(),
            waiting: if task_outstanding {
                Arc::new(tokio::sync::Notify::new())
            } else {
                waiting.clone()
            },
            first_turn: vec![task("child", "Scout"), usage(50_000)],
        },
    )
    .erase();
    let child = if task_outstanding {
        rig_core::driver::Model::new(
            rig_core::test_utils::MockScript::default().with_id("cancel-child"),
            HoldParent {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                waiting: waiting.clone(),
                first_turn: vec![
                    Event::tool_call("read", "Read", json!({"file_path":"state.json"})),
                    usage(30),
                ],
            },
        )
        .erase()
    } else {
        named_mock("cancel-child", [done(30)]).erase()
    };
    let child_model = child.id().unwrap().to_owned();
    assert_eq!(child_model, "cancel-child");
    assert_ne!(model.id().unwrap(), child_model);
    let mut ctx = test_ctx(Some(dir.path().to_owned()), None);
    ctx.idle_timeout = 30.0;
    ctx.params.gremlin_id = Some(run.clone());
    ctx.params.output_corr_id = Some(
        CorrId::new()
            .with_run(&run)
            .with_stage("plan")
            .with_stage_run_id("cancel-execution"),
    );
    ctx.prepopulated_conversation = crate::conversation::new_blank_conversation("hi", false, None);
    ctx.params.task_models = TaskModelSelector::new(
        HashMap::from([(TaskKind::Scout, "child".into())]),
        sync_factory(move |_| Ok(selected(child.clone()))),
    );
    let cancel = CancelToken::new();
    let handle = tokio::spawn(run_agent_loop(
        model,
        "delegate",
        ctx,
        cancel.clone(),
        loop_opts(None),
        InteractiveSession::unattached(),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting.notified())
        .await
        .unwrap();
    wait_for_projection(dir.path(), 2, 2).await;
    assert_live_totals(dir.path());
    assert!(!handle.is_finished());
    // In the outstanding case the parent has not resumed from Task dispatch.
    assert_eq!(
        parent_calls.load(std::sync::atomic::Ordering::SeqCst),
        if task_outstanding { 1 } else { 2 }
    );
    assert!(records(dir.path())
        .iter()
        .any(|r| !r.usage.agent_id.is_empty() && r.usage.usage.prompt_tokens == Some(30)));
    cancel.cancel();
    assert_live_totals(dir.path());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    wait_for_projection(dir.path(), 3, 2).await;
    assert_live_totals(dir.path());
    let records = records(dir.path());
    assert_eq!(records.len(), 3);
    assert_eq!(
        records
            .iter()
            .map(|r| &r.usage.request_id)
            .collect::<HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(records.iter().filter(|r| !r.usage.completed).count(), 1);
    let incomplete = records.iter().find(|r| !r.usage.completed).unwrap();
    assert_eq!(!incomplete.usage.agent_id.is_empty(), task_outstanding);
    assert!(!incomplete.usage.reported);
    let child = records
        .iter()
        .find(|r| r.usage.usage.prompt_tokens == Some(30))
        .unwrap();
    assert_eq!(child.usage.model, child_model);
    assert_eq!(child.usage.usage.cached_input_tokens, Some(15));
    assert!(records
        .iter()
        .all(|r| r.stage_path == "plan" && r.corr_id.stage_run_id() == Some("cancel-execution")));
    let state: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(state["token_usage"]["prompt_tokens"], 50_030);
    assert_eq!(state["token_usage"]["cached_input_tokens"], 25_015);
    assert_eq!(state["token_usage"]["completion_tokens"], 5_003);
    assert_eq!(state["token_usage"]["turns"], 2);
    assert_eq!(
        records
            .iter()
            .filter_map(|r| r.usage.usage.prompt_tokens)
            .sum::<u64>(),
        50_030
    );
}
