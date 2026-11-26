use super::{loop_opts, run_agent_loop, test_ctx, RunContext};
use crate::clients::cancel::CancelToken;
use crate::clients::execution_input::{
    Delivery, ExecutionIdentity, InputControl, InputRegistry, InputRejected,
};
use crate::clients::interactive::{InteractiveSession, InteractiveSessionFactory};
use crate::conversation::new_blank_conversation;
use futures::{stream, StreamExt};
use rig_core::completion::{CompletionRequest, Message};
use rig_core::driver::{Exchange, Model, Opened, Opening, Transport};
use rig_core::test_utils::{MockFrame, MockScript, MockStreamEvent as Event};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(40);
const PROMPT: &str = "Describe the implementation.";
const PARTIAL: &str = "Partial answer. ";

fn finish() -> Vec<Event> {
    vec![
        Event::tool_call("done", "Done", json!({"summary":"finished"})),
        Event::final_response_with_default_usage(),
    ]
}

// Drain a prefix, then signal a pending tail.
#[derive(Clone, Default)]
struct Scripted {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    stalled: Arc<Notify>,
    release: Arc<Notify>,
    held: bool,
    write_after_hold: bool,
    held_tools: bool,
    // Additional nonterminal turns for queued-step regressions.
    extra_write_turns: usize,
}

impl Transport<MockScript> for Scripted {
    fn send(&self, request: CompletionRequest, _: Exchange) -> Opening<MockFrame> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        let n = requests.len();
        if self.held_tools && n == 1 {
            let events = vec![
                Event::tool_call(
                    "effect",
                    "Bash",
                    json!({"command":"printf 'effect\\n' >> effects.txt", "timeout":10}),
                ),
                Event::tool_call(
                    "held",
                    "Bash",
                    json!({"command":"printf 'started\\n' >> starts.txt; while [ ! -f release ]; do sleep 0.01; done; printf 'completed\\n'", "timeout":10}),
                ),
                Event::final_response_with_default_usage(),
            ];
            return Opening::ready(Opened::new(
                stream::iter(events).map(MockFrame::Event).map(Ok),
            ));
        }
        if self.held && n == 1 {
            let stalled = self.stalled.clone();
            let release = self.release.clone();
            let prefix = stream::iter([Ok(MockFrame::Event(Event::text(PARTIAL)))]);
            let tail = stream::once(async move {
                stalled.notify_one();
                release.notified().await;
                Ok(MockFrame::Event(Event::tool_call(
                    "done",
                    "Done",
                    json!({"summary":"finished"}),
                )))
            });
            return Opening::ready(Opened::new(prefix.chain(tail).chain(stream::iter([Ok(
                MockFrame::Event(Event::final_response_with_default_usage()),
            )]))));
        }
        let write_turn = if self.held { 2 } else { 1 };
        let events = if !self.held_tools
            && (!self.held || self.write_after_hold)
            && (write_turn..=write_turn + self.extra_write_turns).contains(&n)
        {
            let tool = Event::tool_call(
                "write",
                "Write",
                json!({"file_path":"stepped.txt", "content":"one step"}),
            );
            vec![tool, Event::final_response_with_default_usage()]
        } else {
            assert!(
                n <= (if self.write_after_hold { 3 } else { 2 }) + self.extra_write_turns,
                "unexpected request #{n}"
            );
            finish()
        };
        Opening::ready(Opened::new(
            stream::iter(events).map(MockFrame::Event).map(Ok),
        ))
    }
}

fn identity(run: &str, stage_run: &str) -> ExecutionIdentity {
    ExecutionIdentity {
        run_id: run.into(),
        stage_name: "same-stage".into(),
        stage_run_id: stage_run.into(),
    }
}

fn context(dir: &std::path::Path, run: &str) -> RunContext {
    let mut ctx = test_ctx(Some(dir.to_owned()), None);
    ctx.idle_timeout = 30.0;
    ctx.params.idle_timeout = Some(30.0);
    ctx.params.gremlin_id = Some(run.into());
    ctx.params.system_prompt = Some("Keep these system instructions unchanged.".into());
    ctx.prepopulated_conversation = new_blank_conversation(PROMPT, false, None);
    ctx
}

fn session(
    factory: &mut InteractiveSessionFactory,
    registry: &InputRegistry,
    id: &ExecutionIdentity,
    cancel: &CancelToken,
) -> InteractiveSession {
    let mut session = factory
        .next_session_for_stage(&id.stage_run_id, true, Some(cancel))
        .unwrap();
    session.bind_input(registry, id.clone(), true, Some(cancel));
    session
}

async fn wait_for_tool_batch(dir: &std::path::Path) {
    timeout(WAIT, async {
        while !dir.join("starts.txt").exists() || !dir.join("effects.txt").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn held_tool_correction_waits_for_batch_without_repeating_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("input-held-tool-correction", "first");
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    let transport = Scripted {
        held_tools: true,
        ..Default::default()
    };
    let mut agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &id.run_id),
        cancel,
        loop_opts(None),
        session,
    ));
    wait_for_tool_batch(dir.path()).await;
    let sender = handle.input_sender(&id).unwrap();
    let mut receipt = sender
        .try_send(
            "correction".into(),
            "Incorporate the completed tool results.".into(),
        )
        .unwrap();
    drop(sender);
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(
        receipt.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    std::fs::write(dir.path().join("release"), "").unwrap();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effects.txt")).unwrap(),
        "effect\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("starts.txt")).unwrap(),
        "started\n"
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let history: Vec<_> = requests[1]
        .chat_history
        .iter()
        .map(|m| m.as_ref().clone())
        .collect();
    let rendered = format!("{history:?}");
    assert_eq!(
        rendered
            .matches("[operator]: Incorporate the completed tool results.")
            .count(),
        1
    );
    assert!(rendered.contains("completed"));
    assert!(rendered.find("completed").unwrap() < rendered.find("[operator]:").unwrap());
    assert!(rendered.contains("effect"));
}

#[tokio::test]
async fn held_tool_cancellation_and_retirement_wake_with_queued_correction() {
    for termination in ["cancel", "drop", "replace"] {
        let dir = tempfile::tempdir().unwrap();
        let registry = InputRegistry::default();
        let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
        handle.set_input_registry(registry.clone());
        let id = identity(&format!("input-held-tool-{termination}"), "first");
        let cancel = CancelToken::new();
        let session = session(&mut factory, &registry, &id, &cancel);
        let transport = Scripted {
            held_tools: true,
            ..Default::default()
        };
        let mut agent = tokio::spawn(run_agent_loop(
            Model::new(MockScript::default(), transport.clone()).erase(),
            PROMPT,
            context(dir.path(), &id.run_id),
            cancel.clone(),
            loop_opts(None),
            session,
        ));
        wait_for_tool_batch(dir.path()).await;
        let sender = handle.input_sender(&id).unwrap();
        let mut receipt = sender
            .try_send("correction".into(), "Do not replay tools.".into())
            .unwrap();
        assert!(timeout(QUIET, &mut agent).await.is_err());
        assert_eq!(
            receipt.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        let _successor = match termination {
            "cancel" => {
                cancel.cancel();
                None
            }
            "drop" => {
                drop(factory);
                None
            }
            _ => Some(
                factory
                    .next_session_for_stage("replacement", true, None)
                    .unwrap(),
            ),
        };
        assert!(matches!(
            timeout(WAIT, agent).await.unwrap().unwrap(),
            Err(crate::clients::error::ClientError::Cancelled)
        ));
        assert_eq!(cancel.is_cancelled(), termination == "cancel");
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        assert!(!dir.path().join("release").exists());
        assert!(handle.input_sender(&id).is_none());
    }
}

#[tokio::test]
async fn opening_and_dropping_native_handles_does_not_interrupt_or_request() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("input-no-watchers", "first");
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    let transport = Scripted {
        held: true,
        ..Default::default()
    };
    let mut agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &id.run_id),
        cancel,
        loop_opts(None),
        session,
    ));
    timeout(WAIT, transport.stalled.notified()).await.unwrap();
    let first = handle.input_sender(&id).unwrap();
    let second = handle.clone().input_sender(&id).unwrap();
    drop(first);
    drop(second);
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    transport.release.notify_one();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_pause_survives_sender_drop_step_is_one_turn_and_resume_progresses() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("input-pause-step-resume", "first");
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    let transport = Scripted {
        held: true,
        write_after_hold: true,
        ..Default::default()
    };
    let mut agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &id.run_id),
        cancel,
        loop_opts(None),
        session,
    ));
    timeout(WAIT, transport.stalled.notified()).await.unwrap();
    let first = handle.input_sender(&id).unwrap();
    let second = handle.clone().input_sender(&id).unwrap();
    drop(first);
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    let pause = second
        .try_control("pause".into(), InputControl::Pause)
        .unwrap();
    drop(second);
    assert_eq!(
        timeout(WAIT, pause).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(!dir.path().join("stepped.txt").exists());
    let sender = handle.clone().input_sender(&id).unwrap();
    let step = sender
        .try_control("step".into(), InputControl::Step)
        .unwrap();
    assert_eq!(
        timeout(WAIT, step).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    timeout(WAIT, async {
        while !dir.path().join("stepped.txt").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("stepped.txt")).unwrap(),
        "one step"
    );
    let resume = sender
        .try_control("resume".into(), InputControl::Resume)
        .unwrap();
    assert_eq!(
        timeout(WAIT, resume).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn held_partial_correction_receipt_is_separate_and_unread_progress_does_not_block() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("input-partial-correction", "first");
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    // Subscribe but never consume progress: delivery is not an event acknowledgement.
    let (sub, _unread) = gremlins_magic_bus::subscribe_run(&id.run_id);
    let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
    let transport = Scripted {
        held: true,
        ..Default::default()
    };
    let agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &id.run_id),
        cancel,
        loop_opts(None),
        session,
    ));
    timeout(WAIT, transport.stalled.notified()).await.unwrap();
    let sender = handle.input_sender(&id).unwrap();
    let mut receipt = sender
        .try_send("correction".into(), "Use the new implementation.".into())
        .unwrap();
    assert_eq!(
        receipt.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    drop(sender);
    // Leave the receipt unread until after execution has ended.
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(receipt.try_recv(), Ok(Delivery::Delivered));
    assert!(handle.input_sender(&id).is_none());
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let history: Vec<_> = requests[1]
        .chat_history
        .iter()
        .map(|m| m.as_ref().clone())
        .collect();
    assert!(history.windows(2).any(|pair| pair
        == [
            Message::assistant(PARTIAL),
            Message::user("[operator]: Use the new implementation.")
        ]));
    let rendered = format!("{history:?}");
    assert_eq!(
        rendered
            .matches("[operator]: Use the new implementation.")
            .count(),
        1
    );
    let systems = |request: &CompletionRequest| {
        request
            .chat_history
            .iter()
            .filter_map(|m| match m.as_ref() {
                Message::System { content } => Some(content.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(systems(&requests[0]), systems(&requests[1]));
}

#[tokio::test]
async fn inbox_bail_is_control_but_correction_bail_is_literal_text() {
    for bail_control in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let registry = InputRegistry::default();
        let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
        handle.set_input_registry(registry.clone());
        let id = identity(
            if bail_control {
                "input-bail"
            } else {
                "input-literal-bail"
            },
            "first",
        );
        let cancel = CancelToken::new();
        let session = session(&mut factory, &registry, &id, &cancel);
        let transport = Scripted {
            held: true,
            ..Default::default()
        };
        let agent = tokio::spawn(run_agent_loop(
            Model::new(MockScript::default(), transport.clone()).erase(),
            PROMPT,
            context(dir.path(), &id.run_id),
            cancel,
            loop_opts(None),
            session,
        ));
        timeout(WAIT, transport.stalled.notified()).await.unwrap();
        let sender = handle.input_sender(&id).unwrap();
        let receipt = if bail_control {
            sender.try_control("bail".into(), InputControl::Bail)
        } else {
            sender.try_send("bail-text".into(), "bail".into())
        }
        .unwrap();
        drop(sender);
        assert_eq!(
            timeout(WAIT, receipt).await.unwrap().unwrap(),
            Delivery::Delivered
        );
        let result = timeout(WAIT, agent).await.unwrap().unwrap();
        let requests = transport.requests.lock().unwrap();
        if bail_control {
            assert!(matches!(
                result,
                Err(crate::clients::error::ClientError::Bail { reason })
                    if reason == "operator bailed"
            ));
            assert_eq!(requests.len(), 1);
        } else {
            result.unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[1]
                .chat_history
                .iter()
                .any(|message| message.as_ref() == &Message::user("[operator]: bail")));
        }
        assert!(handle.input_sender(&id).is_none());
    }
}

#[tokio::test]
async fn queued_then_cancelled_before_poll_never_reaches_same_named_successor() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let old_id = identity("input-cancel-before-poll", "old");
    let cancel = CancelToken::new();
    let old = session(&mut factory, &registry, &old_id, &cancel);
    let stale = handle.input_sender(&old_id).unwrap();
    let mut receipt = stale
        .try_send("old-command".into(), "MUST NOT REACH MODEL".into())
        .unwrap();
    let transport = Scripted {
        held: true,
        ..Default::default()
    };
    cancel.cancel();
    assert_eq!(receipt.try_recv(), Ok(Delivery::NotDelivered));
    let result = timeout(
        WAIT,
        run_agent_loop(
            Model::new(MockScript::default(), transport.clone()).erase(),
            PROMPT,
            context(dir.path(), &old_id.run_id),
            cancel,
            loop_opts(None),
            old,
        ),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(crate::clients::error::ClientError::Cancelled)
    ));
    assert!(transport.requests.lock().unwrap().is_empty());
    let new_id = identity(&old_id.run_id, "new");
    let cancel = CancelToken::new();
    let new = session(&mut factory, &registry, &new_id, &cancel);
    assert!(handle.input_sender(&old_id).is_none());
    assert!(matches!(
        stale.try_send("late".into(), "old".into()),
        Err(InputRejected::Revoked)
    ));
    let agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &new_id.run_id),
        cancel,
        loop_opts(None),
        new,
    ));
    timeout(WAIT, transport.stalled.notified()).await.unwrap();
    transport.release.notify_one();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!format!("{:?}", requests[0]).contains("MUST NOT REACH MODEL"));
}

#[tokio::test]
async fn two_queued_inbox_steps_grant_exactly_two_turns() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("inbox-queued-steps", "first");
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    let transport = Scripted {
        held: true,
        write_after_hold: true,
        extra_write_turns: 1,
        ..Default::default()
    };
    let mut agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &id.run_id),
        cancel,
        loop_opts(None),
        session,
    ));
    timeout(WAIT, transport.stalled.notified()).await.unwrap();
    let sender = handle.input_sender(&id).unwrap();
    let pause = sender
        .try_control("pause".into(), InputControl::Pause)
        .unwrap();
    assert_eq!(
        timeout(WAIT, pause).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    // No await between sends: both permits must survive draining one inbox batch.
    let first = sender
        .try_control("step-1".into(), InputControl::Step)
        .unwrap();
    let second = sender
        .try_control("step-2".into(), InputControl::Step)
        .unwrap();
    assert_eq!(
        timeout(WAIT, first).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert_eq!(
        timeout(WAIT, second).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    timeout(WAIT, async {
        while transport.requests.lock().unwrap().len() < 3 {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
    let resume = sender
        .try_control("resume".into(), InputControl::Resume)
        .unwrap();
    assert_eq!(
        timeout(WAIT, resume).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn factory_drop_retires_inbox_paused_agent_without_token_cancellation() {
    retire_inbox_paused_agent(true).await;
}

#[tokio::test]
async fn factory_replacement_retires_inbox_paused_agent_without_token_cancellation() {
    retire_inbox_paused_agent(false).await;
}

async fn retire_inbox_paused_agent(drop_factory: bool) {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity(
        if drop_factory {
            "inbox-retire-drop"
        } else {
            "inbox-retire-replace"
        },
        "first",
    );
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    let transport = Scripted {
        held: true,
        ..Default::default()
    };
    let mut agent = tokio::spawn(run_agent_loop(
        Model::new(MockScript::default(), transport.clone()).erase(),
        PROMPT,
        context(dir.path(), &id.run_id),
        cancel.clone(),
        loop_opts(None),
        session,
    ));
    timeout(WAIT, transport.stalled.notified()).await.unwrap();
    let sender = handle.input_sender(&id).unwrap();
    let pause = sender
        .try_control("pause".into(), InputControl::Pause)
        .unwrap();
    assert_eq!(
        timeout(WAIT, pause).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    let _successor = if drop_factory {
        drop(factory);
        None
    } else {
        Some(
            factory
                .next_session_for_stage("replacement", true, None)
                .unwrap(),
        )
    };
    assert!(matches!(
        timeout(WAIT, agent).await.unwrap().unwrap(),
        Err(crate::clients::error::ClientError::Cancelled)
    ));
    assert!(!cancel.is_cancelled());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(handle.input_sender(&id).is_none());
    assert!(matches!(
        sender.try_control("stale".into(), InputControl::Resume),
        Err(InputRejected::Revoked)
    ));
}
