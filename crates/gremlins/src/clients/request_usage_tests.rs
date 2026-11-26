//! Headless tests; included as a child of `request_usage` by the parent module.
use super::*;
use crate::clients::backends::anthropic::capture_compaction;
use futures::{stream, StreamExt};
use gremlins_magic_bus::{init_bus, subscribe_run, ReceiverHalf, TryRecvError};
use rig_core::error::{ErrorKind, ErrorReport};
use rig_core::message::Origin;
use rig_core::streaming::Relayed;
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;

fn corr() -> CorrId {
    CorrId::new()
        .with_run(uuid::Uuid::new_v4().to_string())
        .with_stage("implement")
        .with_stage_run_id("stage-instance")
        .with_turn(7)
        .with_turn_token("caller-turn")
        .with_attempt(2)
}

fn response(usage: Usage, raw: Value) -> CompletionResponse {
    CompletionResponse::new(
        vec![],
        usage,
        Origin::new("test-wire", "provider", "model"),
        raw,
    )
}

fn done(corr: &CorrId, chain: &str, response: CompletionResponse) -> UsageStream {
    UsageStream::new(
        Streamed::relay(
            "headless",
            Box::pin(stream::iter([Ok(Relayed::Done(Arc::new(response)))])),
        ),
        RequestUsage::new(corr, "provider", "model", chain),
    )
}

fn published(rx: &mut ReceiverHalf<EventKind>, corr: &CorrId) -> ModelCallUsage {
    let event = rx
        .try_recv()
        .expect("usage published synchronously on drop");
    assert_eq!(event.corr_id(), corr);
    match event.into_kind() {
        EventKind::ModelCallUsage { usage } => usage,
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn compaction_cancelled_before_response_keeps_request_kind() {
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let mut owner = RequestUsage::new(&corr, "anthropic", "model", "");
    owner.mark_compaction();
    drop(owner);
    let record = published(&mut rx, &corr);
    assert_eq!(record.request_kind, "compaction");
    assert!(!record.completed);
    assert!(!record.reported);
}

#[tokio::test]
async fn compaction_iterations_are_charged_without_summary_prose() {
    init_bus();
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let raw = json!({
        "content": [{"type": "compaction", "content": null, "signature": "secret"}],
        "usage": {"input_tokens": 0, "output_tokens": 0, "iterations": [
            {"type": "message", "input_tokens": 99999, "output_tokens": 99999},
            {"type": "compaction", "input_tokens": 100, "output_tokens": 20,
             "cache_read_input_tokens": 40, "cache_creation_input_tokens": 10},
            {"type": "compaction", "input_tokens": 50, "output_tokens": 5}
        ]}
    });
    let mut owner = RequestUsage::new(&corr, "anthropic", "selected-model", "parent/child");
    owner.mark_compaction();
    capture_compaction(&mut owner, &raw, true);
    drop(owner);
    let record = published(&mut rx, &corr);
    assert!(record.completed && record.reported);
    assert_eq!(record.agent_id, "parent/child");
    assert_eq!(record.model, "selected-model");
    assert_eq!(record.request_kind, "compaction");
    assert_eq!(record.usage.turns, 0);
    assert_eq!(record.usage.prompt_tokens, Some(200));
    assert_eq!(record.usage.completion_tokens, Some(25));
    assert_eq!(record.usage.cached_input_tokens, Some(40));
    assert_eq!(record.usage.cache_creation_input_tokens, Some(10));
    assert_eq!(record.raw, json!({"usage": raw["usage"]}));
    empty(&mut rx);
}

#[tokio::test]
async fn compaction_then_agent_turn_persists_one_turn_and_both_requests() {
    use crate::executor::accounting::AccountingCollector;
    use crate::executor::state::FileSystemStateStore;

    let corr = corr();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("state.json"), "{}").unwrap();
    let store = Arc::new(FileSystemStateStore::from_path(dir.path().to_path_buf()));
    let collector = AccountingCollector::start(corr.run().unwrap(), store).unwrap();
    collector
        .register_execution(corr.stage_run_id().unwrap(), "pipeline/agent")
        .unwrap();

    let mut compaction = RequestUsage::new(&corr, "anthropic", "model", "agent");
    compaction.mark_compaction();
    capture_compaction(
        &mut compaction,
        &json!({
            "usage": {"iterations": [{"type": "compaction",
                "input_tokens": 100, "output_tokens": 20,
                "cache_read_input_tokens": 40, "cache_creation_input_tokens": 10}]}
        }),
        true,
    );
    drop(compaction);
    let mut usage = Usage::default();
    usage.input_tokens = Some(30);
    usage.output_tokens = Some(5);
    let mut agent = done(&corr, "agent", response(usage, json!({})));
    while agent.next().await.is_some() {}
    drop(agent);
    collector.flush().await.unwrap();

    let read = |name: &str| -> Value {
        serde_json::from_slice(&std::fs::read(dir.path().join(name)).unwrap()).unwrap()
    };
    let run = read("state.json");
    let execution: Value = serde_json::from_str(
        std::fs::read_to_string(dir.path().join("token_usage.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    for summary in [&run["token_usage"], &execution] {
        assert_eq!(summary["turns"], 1);
        assert_eq!(summary["prompt_tokens"], 180);
        assert_eq!(summary["completion_tokens"], 25);
        assert_eq!(summary["cached_input_tokens"], 40);
        assert_eq!(summary["cache_creation_input_tokens"], 10);
    }
    let journal = read("native_accounting.json");
    let records = journal["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_ne!(
        records[0]["usage"]["request_id"],
        records[1]["usage"]["request_id"]
    );
    for record in records {
        assert_eq!(record["usage"]["completed"], true);
        assert_eq!(record["usage"]["reported"], true);
    }
}

#[tokio::test]
async fn compaction_missing_counters_do_not_use_zero_top_level_totals() {
    init_bus();
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let mut owner = RequestUsage::new(&corr, "anthropic", "model", "");
    owner.mark_compaction();
    capture_compaction(
        &mut owner,
        &json!({"usage": {"input_tokens": 0, "output_tokens": 0}}),
        true,
    );
    drop(owner);
    let record = published(&mut rx, &corr);
    assert!(record.completed);
    assert!(!record.reported);
    assert_eq!(record.usage.turns, 0);
    assert_eq!(record.usage.prompt_tokens, None);
    assert_eq!(record.usage.completion_tokens, None);
    empty(&mut rx);
}

#[test]
fn compaction_error_json_retains_charges_without_completed_turn() {
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let raw = json!({
        "type": "error", "error": {"message": "private HTTP failure"},
        "usage": {"iterations": [{"type": "compaction", "input_tokens": 100,
            "output_tokens": 20, "cache_read_input_tokens": 40}]}
    });
    let mut owner = RequestUsage::new(&corr, "anthropic", "model", "");
    owner.mark_compaction();
    capture_compaction(&mut owner, &raw, true);
    drop(owner);
    let record = published(&mut rx, &corr);
    assert!(!record.completed);
    assert!(record.reported);
    assert_eq!(record.usage.turns, 0);
    assert_eq!(record.usage.prompt_tokens, Some(140));
    assert_eq!(record.usage.completion_tokens, Some(20));
    assert_eq!(record.usage.cached_input_tokens, Some(40));
    assert_eq!(record.raw, json!({"usage": raw["usage"]}));
    assert!(!serde_json::to_string(&record).unwrap().contains("private"));
    empty(&mut rx);
}

#[test]
fn compaction_error_without_usage_does_not_invent_charges_or_turns() {
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let mut owner = RequestUsage::new(&corr, "anthropic", "model", "");
    owner.mark_compaction();
    capture_compaction(
        &mut owner,
        &json!({"error": {"message": "private failure"}}),
        true,
    );
    drop(owner);
    let record = published(&mut rx, &corr);
    assert!(!record.completed);
    assert!(!record.reported);
    assert_eq!(
        serde_json::to_value(record.usage).unwrap(),
        json!({"turns": 0})
    );
    assert_eq!(record.raw, serde_json::to_value(Usage::default()).unwrap());
    empty(&mut rx);
}

#[tokio::test]
async fn final_observation_is_request_local_and_only_available_after_drop() {
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let mut stream = done(
        &corr,
        "same-chain",
        response(Usage::new().input_tokens(17), json!({})),
    );
    let observation = stream.observation();
    while stream.next().await.is_some() {}
    assert!(observation.take().is_none());
    empty(&mut rx);
    let mut unrelated = RequestUsage::new(&corr, "provider", "model", "same-chain");
    unrelated.capture_usage(&Usage::new().input_tokens(999), &json!({}), false, false);
    drop(unrelated);
    drop(stream);
    assert_eq!(observation.take().unwrap().input_tokens, Some(17));
    assert!(observation.take().is_none());
    assert_eq!(published(&mut rx, &corr).usage.prompt_tokens, Some(999));
    assert_eq!(published(&mut rx, &corr).usage.prompt_tokens, Some(17));
    empty(&mut rx);
}

#[tokio::test]
async fn stream_preserves_all_usage_metadata_keys_together() {
    let corr = corr();
    let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
    let raw = json!({
        "usage": {"input_tokens": 17},
        "usageMetadata": {"custom": 1},
        "usage_metadata": {"custom": 2},
        "copilot_usage": {"billing": 3},
        "content": "private"
    });
    let mut stream = done(
        &corr,
        "",
        response(Usage::new().input_tokens(17), raw.clone()),
    );
    while stream.next().await.is_some() {}
    drop(stream);
    let record = published(&mut rx, &corr);
    let mut expected = raw;
    expected.as_object_mut().unwrap().remove("content");
    assert_eq!(record.raw, expected);
    empty(&mut rx);
}

fn empty(rx: &mut ReceiverHalf<EventKind>) {
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn success_keeps_usage_subsets_and_only_usage_raw_fields() {
    for key in ["usage", "usageMetadata", "usage_metadata"] {
        let corr = corr();
        let (sub, mut rx) = subscribe_run(corr.run().unwrap());
        let usage = Usage::new()
            .input_tokens(60_000)
            .output_tokens(2_000)
            .cached_input_tokens(40_000)
            .cache_creation_input_tokens(10_000)
            .reasoning_tokens(500);
        let mut raw = json!({
            "messages": ["private prompt"],
            "choices": [{"text": "private answer"}],
            "error": {"message": "private error"}
        });
        raw[key] = json!({"provider_counter": 42});
        let mut stream = done(&corr, "[task.a3f1.b72e]", response(usage, raw));
        assert!(stream.next().await.is_none());
        empty(&mut rx);
        drop(stream);
        let record = published(&mut rx, &corr);
        assert!(record.completed);
        assert!(record.reported);
        assert_eq!(record.usage.prompt_tokens, Some(60_000));
        assert_eq!(record.usage.completion_tokens, Some(2_000));
        assert_eq!(record.usage.cached_input_tokens, Some(40_000));
        assert_eq!(record.usage.cache_creation_input_tokens, Some(10_000));
        assert_eq!(record.usage.reasoning_tokens, Some(500));
        assert_eq!(record.usage.turns, 1);
        assert_eq!(record.raw, json!({(key): {"provider_counter": 42}}));
        assert!(!record.request_id.is_empty());
        assert_eq!(record.provider, "provider");
        assert_eq!(record.model, "model");
        assert_eq!(record.agent_id, "[task.a3f1.b72e]");
        assert!(!serde_json::to_string(&record).unwrap().contains("private"));
        empty(&mut rx);
        init_bus().unsubscribe(&sub);
    }
}

#[tokio::test]
async fn missing_usage_stays_missing_and_fallback_never_copies_response() {
    for usage in [
        Usage::default(),
        Usage::new().input_tokens(0).output_tokens(0),
    ] {
        let corr = corr();
        let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
        let expected = serde_json::to_value(usage).unwrap();
        let reported = usage.is_reported();
        let mut stream = done(
            &corr,
            "root",
            response(usage, json!({"content": "private", "error": "private"})),
        );
        assert!(stream.next().await.is_none());
        drop(stream);
        let record = published(&mut rx, &corr);
        assert!(record.completed);
        assert_eq!(record.reported, reported);
        assert_eq!(record.raw, expected);
        assert_eq!(
            record.usage.prompt_tokens,
            if reported { Some(0) } else { None }
        );
        assert_eq!(
            record.usage.completion_tokens,
            if reported { Some(0) } else { None }
        );
        assert_eq!(record.usage.turns, 1);
    }
}

#[test]
fn dropping_polled_pending_owner_publishes_one_incomplete_request() {
    let corr = corr();
    let (sub, mut rx) = subscribe_run(corr.run().unwrap());
    let usage = RequestUsage::new(&corr, "provider", "model", "[fork.a3f1]");
    let id = usage.record.as_ref().unwrap().request_id.clone();
    let mut stream = UsageStream::new(
        Streamed::relay("headless", Box::pin(stream::pending())),
        usage,
    );
    let mut future = Box::pin(async move { stream.next().await });
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(future.as_mut().poll(&mut cx).is_pending());
    empty(&mut rx);
    drop(future);
    let record = published(&mut rx, &corr);
    assert_eq!(record.request_id, id);
    assert_eq!(record.agent_id, "[fork.a3f1]");
    assert!(!record.completed);
    assert!(!record.reported);
    assert_eq!(record.usage.turns, 0);
    assert_eq!(record.usage.prompt_tokens, None);
    assert_eq!(record.usage.completion_tokens, None);
    empty(&mut rx);
    init_bus().unsubscribe(&sub);
}

#[tokio::test]
async fn aborting_pending_task_publishes_one_incomplete_request() {
    let corr = corr();
    let (sub, mut rx) = subscribe_run(corr.run().unwrap());
    let usage = RequestUsage::new(&corr, "provider", "model", "[task.aborted]");
    let id = usage.record.as_ref().unwrap().request_id.clone();
    let (ready, started) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut stream = UsageStream::new(
            Streamed::relay("headless", Box::pin(stream::pending())),
            usage,
        );
        let mut next = Box::pin(stream.next());
        assert!(futures::poll!(next.as_mut()).is_pending());
        ready.send(()).unwrap();
        next.await
    });
    started.await.unwrap();
    empty(&mut rx);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let record = published(&mut rx, &corr);
    assert_eq!(record.request_id, id);
    assert_eq!(record.agent_id, "[task.aborted]");
    assert!(!record.completed);
    assert!(!record.reported);
    assert_eq!(record.usage.turns, 0);
    assert_eq!(record.usage.prompt_tokens, None);
    empty(&mut rx);
    init_bus().unsubscribe(&sub);
}

#[tokio::test]
async fn stream_error_and_retry_have_fresh_ids_and_same_chain_and_corr() {
    let corr = corr();
    let (sub, mut rx) = subscribe_run(corr.run().unwrap());
    let error = ErrorReport::new(ErrorKind::Other, "private provider failure").with_retryable(true);
    let mut failed = UsageStream::new(
        Streamed::relay("headless", Box::pin(stream::iter([Err(error)]))),
        RequestUsage::new(&corr, "provider", "model", "[task.a3f1]"),
    );
    assert!(failed.next().await.unwrap().is_err());
    assert!(failed.next().await.is_none());
    assert!(failed.failed);
    assert!(!failed.usage.record.as_ref().unwrap().completed);
    empty(&mut rx);
    drop(failed);
    let first = published(&mut rx, &corr);
    assert!(!first.completed);
    assert_eq!(first.usage.turns, 0);
    assert!(!serde_json::to_string(&first).unwrap().contains("private"));

    let mut retry = done(
        &corr,
        "[task.a3f1]",
        response(Usage::new().input_tokens(12), Value::Null),
    );
    assert_ne!(
        retry.usage.record.as_ref().unwrap().request_id,
        first.request_id
    );
    assert!(retry.next().await.is_none());
    drop(retry);
    let second = published(&mut rx, &corr);
    assert_ne!(first.request_id, second.request_id);
    assert_eq!(first.agent_id, second.agent_id);
    assert!(second.completed);
    assert!(second.reported);
    assert_eq!(second.usage.prompt_tokens, Some(12));
    assert_eq!(second.usage.turns, 1);
    empty(&mut rx);
    init_bus().unsubscribe(&sub);
}

#[tokio::test]
async fn provider_error_or_abort_keeps_reported_usage_without_completed_turn() {
    for aborted in [false, true] {
        let mut response = response(
            Usage::new().input_tokens(25),
            json!({"usage": {"input_tokens": 25}, "error": "private failure"}),
        );
        if aborted {
            response.aborted = Some("private cancellation".into());
        } else {
            response.error = Some("private failure".into());
        }
        let corr = corr();
        let (_sub, mut rx) = subscribe_run(corr.run().unwrap());
        let mut stream = done(&corr, "[task.a3f1]", response);
        while let Some(item) = stream.next().await {
            let _ = item;
        }
        drop(stream);
        let record = published(&mut rx, &corr);
        assert!(!record.completed);
        assert!(record.reported);
        assert_eq!(record.usage.prompt_tokens, Some(25));
        assert_eq!(record.usage.turns, 0);
        assert_eq!(record.raw, json!({"usage": {"input_tokens": 25}}));
    }
}

// These frames go through the production wire decoders, not synthetic normalized
// CompletionResponses: upgrading rig must not silently change what we persist.
fn decode_usage_fixture<W>(wire: &W, frames: Vec<Value>) -> CompletionResponse
where
    W: rig_core::wire::Wire<Op = Completion, Frame = rig_core::wire::WireFrame>,
{
    rig_core::wire::decode_reply(
        wire,
        &rig_core::completion::CompletionRequest::new("private prompt"),
        rig_core::wire::Mode::Streaming,
        frames
            .into_iter()
            .map(|frame| rig_core::wire::WireFrame::Text(frame.to_string())),
    )
    .unwrap()
}

async fn assert_wire_usage(
    corr: &CorrId,
    response: CompletionResponse,
    expected_raw: Value,
    expected: Value,
) -> ModelCallUsage {
    assert_eq!(
        response.usage.input_tokens,
        expected["prompt_tokens"].as_u64()
    );
    assert_eq!(
        response.usage.output_tokens,
        expected["completion_tokens"].as_u64()
    );
    assert_eq!(
        response.usage.cached_input_tokens,
        expected["cached_input_tokens"].as_u64()
    );
    assert_eq!(
        response.usage.cache_creation_input_tokens,
        expected["cache_creation_input_tokens"].as_u64()
    );
    assert_eq!(
        response.usage.reasoning_tokens,
        expected["reasoning_tokens"].as_u64()
    );
    // Verify the wire actually preserved the provider counters before capture.
    for (key, value) in expected_raw.as_object().unwrap() {
        assert_eq!(&response.raw[key], value, "wire raw field {key}");
    }
    let (sub, mut rx) = subscribe_run(corr.run().unwrap());
    let mut stream = done(corr, "[task.fixture]", response);
    assert!(stream.next().await.is_none());
    drop(stream);
    let record = published(&mut rx, corr);
    assert_eq!(record.raw, expected_raw);
    assert_eq!(serde_json::to_value(&record.usage).unwrap(), expected);
    assert!(record.reported);
    assert!(record.completed);
    assert!(!record.raw.to_string().contains("private"));
    empty(&mut rx);
    init_bus().unsubscribe(&sub);
    record
}

fn assert_successive_requests(records: &[ModelCallUsage]) {
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].request_id, records[1].request_id);
    assert_eq!(records[0].agent_id, records[1].agent_id);
    assert_eq!(records[0].usage.prompt_tokens, Some(50_000));
    assert_eq!(records[1].usage.prompt_tokens, Some(60_000));
    assert_eq!(records[1].usage.cached_input_tokens, Some(40_000));
    assert_eq!(
        records
            .iter()
            .map(|r| r.usage.prompt_tokens.unwrap())
            .sum::<u64>(),
        110_000
    );
    assert_eq!(records.iter().map(|r| r.usage.turns).sum::<usize>(), 2);
}

#[tokio::test]
async fn anthropic_stream_wire_keeps_disjoint_cache_and_reasoning_metadata() {
    let corr = corr();
    let mut records = Vec::new();
    for (input, cached) in [(50_000, 0), (60_000, 40_000)] {
        let start = json!({"input_tokens": input - cached - 10_000, "output_tokens": 0,
        "cache_read_input_tokens": cached, "cache_creation_input_tokens": 10_000,
        "cache_creation": {"ephemeral_5m_input_tokens": 7_000, "ephemeral_1h_input_tokens": 3_000}});
        let end =
            json!({"output_tokens": 2_000, "output_tokens_details": {"thinking_tokens": 500}});
        let response = decode_usage_fixture(
            &rig_core::providers::anthropic::AnthropicConfig::new("unused")
                .completion("claude-sonnet-4-6"),
            vec![
                json!({"type": "message_start", "message": {"id": "msg_fixture", "type": "message",
                "role": "assistant", "model": "claude-sonnet-4-6", "content": [], "usage": start}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "private answer"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": end}),
                json!({"type": "message_stop"}),
            ],
        );
        let mut usage = start;
        usage
            .as_object_mut()
            .unwrap()
            .extend(end.as_object().unwrap().clone());
        records.push(assert_wire_usage(
        &corr,
        response,
        json!({"usage": usage}),
        json!({
            "prompt_tokens": input, "completion_tokens": 2_000, "cached_input_tokens": cached,
            "cache_creation_input_tokens": 10_000, "reasoning_tokens": 500, "turns": 1
        }),
    )
    .await);
    }
    assert_successive_requests(&records);
}

#[tokio::test]
async fn chat_stream_wire_keeps_inclusive_usage_and_copilot_billing() {
    use rig_core::providers::openai::wire::{OpenAIConfig, OPENAI};
    for dialect in [&OPENAI, &rig_core::providers::copilot::DIALECT] {
        let corr = corr();
        let mut records = Vec::new();
        for (input, cached) in [(50_000, 0), (60_000, 40_000)] {
            let usage = json!({"prompt_tokens": input, "completion_tokens": 1_000, "total_tokens": input + 1_000,
            "prompt_tokens_details": {"cached_tokens": cached},
            "completion_tokens_details": {"reasoning_tokens": 500}});
            let billing = json!({"total_nano_aiu": 42, "token_details": [{"token_type": "cache_read", "token_count": cached}]});
            let mut terminal = json!({"id": "chat_fixture", "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-4o", "choices": [], "usage": usage});
            let mut raw = json!({"usage": usage});
            if dialect == &rig_core::providers::copilot::DIALECT {
                terminal["copilot_usage"] = billing.clone();
                raw["copilot_usage"] = billing;
            }
            let response = decode_usage_fixture(
                &OpenAIConfig::with_key(dialect, "unused").chat("gpt-4o"),
                vec![
                    json!({"id": "chat_fixture", "object": "chat.completion.chunk", "created": 0,
                "model": "gpt-4o", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "private answer"}, "finish_reason": null}]}),
                    json!({"id": "chat_fixture", "object": "chat.completion.chunk", "created": 0,
                "model": "gpt-4o", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                    terminal,
                ],
            );
            records.push(
                assert_wire_usage(
                    &corr,
                    response,
                    raw,
                    json!({"prompt_tokens": input, "completion_tokens": 1_000,
            "cached_input_tokens": cached, "reasoning_tokens": 500, "turns": 1}),
                )
                .await,
            );
        }
        assert_successive_requests(&records);
    }
}

#[tokio::test]
async fn responses_stream_wire_keeps_usage_and_sibling_copilot_billing() {
    use rig_core::providers::openai::{OpenAIConfig, Route};
    for copilot in [false, true] {
        let config = if copilot {
            OpenAIConfig::with_key(&rig_core::providers::copilot::DIALECT, "unused")
        } else {
            OpenAIConfig::new("unused")
        }
        .with_route(Route::Responses)
        .client();
        let corr = corr();
        let mut records = Vec::new();
        for (input, cached) in [(50_000, 0), (60_000, 40_000)] {
            let usage = json!({"input_tokens": input, "output_tokens": 1_000, "total_tokens": input + 1_000,
            "input_tokens_details": {"cached_tokens": cached},
            "output_tokens_details": {"reasoning_tokens": 500}});
            let mut terminal = json!({"type": "response.completed", "response": {
            "id": "resp_fixture", "object": "response", "status": "completed", "model": "gpt-4o",
            "output": [{"id": "msg_fixture", "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "private answer", "annotations": []}]}], "usage": usage}});
            let mut raw = json!({"usage": usage});
            if copilot {
                let billing = json!({"total_nano_aiu": 123});
                terminal["copilot_usage"] = billing.clone();
                raw["copilot_usage"] = billing;
            }
            let response = decode_usage_fixture(&config.completion("gpt-4o").wire, vec![terminal]);
            records.push(
                assert_wire_usage(
                    &corr,
                    response,
                    raw,
                    json!({"prompt_tokens": input, "completion_tokens": 1_000,
            "cached_input_tokens": cached, "reasoning_tokens": 500, "turns": 1}),
                )
                .await,
            );
        }
        assert_successive_requests(&records);
    }
}
