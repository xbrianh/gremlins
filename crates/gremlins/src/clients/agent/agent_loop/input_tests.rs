use super::{loop_opts, run_agent_loop, test_ctx, RunContext};
use crate::clients::cancel::CancelToken;
use crate::clients::execution_input::{
    Delivery, ExecutionIdentity, InputControl, InputRegistry, InputRejected,
};
use crate::clients::interactive::{
    InteractiveCommand, InteractiveSession, InteractiveSessionFactory,
};
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

// Like interrupted_tests::HeldResponse: drain a prefix, then signal a pending tail.
#[derive(Clone, Default)]
struct Scripted {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    stalled: Arc<Notify>,
    release: Arc<Notify>,
    held: bool,
    write_after_hold: bool,
    held_tool: bool,
    // Additional nonterminal turns for queued-step regressions.
    extra_write_turns: usize,
}

impl Transport<MockScript> for Scripted {
    fn send(&self, request: CompletionRequest, _: Exchange) -> Opening<MockFrame> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        let n = requests.len();
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
        let events = if (!self.held || self.write_after_hold)
            && (write_turn..=write_turn + self.extra_write_turns).contains(&n)
        {
            let tool = if self.held_tool {
                Event::tool_call(
                    "held-tool",
                    "Bash",
                    json!({"command": "touch tool-entered; for i in $(seq 1 1000); do if [ -f tool-release ]; then touch tool-finished; exit 0; fi; sleep 0.01; done; exit 1"}),
                )
            } else {
                Event::tool_call(
                    "write",
                    "Write",
                    json!({"file_path":"stepped.txt", "content":"one step"}),
                )
            };
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
async fn legacy_pause_accepts_inbox_correction_and_bail_without_debugger_action() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("legacy-pause-inbox", "first");
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
    let mut debugger = handle.target().attach_debugger().unwrap();
    timeout(WAIT, &mut debugger.ready).await.unwrap().unwrap();
    let sender = handle.input_sender(&id).unwrap();
    let correction = sender
        .try_send("correction".into(), "Keep waiting.".into())
        .unwrap();
    assert_eq!(
        timeout(WAIT, correction).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    let bail = sender
        .try_control("bail".into(), InputControl::Bail)
        .unwrap();
    assert_eq!(
        timeout(WAIT, bail).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(matches!(timeout(WAIT, agent).await.unwrap().unwrap(),
        Err(crate::clients::error::ClientError::Bail { reason }) if reason == "operator bailed"));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn inbox_pause_accepts_debugger_ready_but_legacy_commands_and_correction_do_not_release_it() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("inbox-pause-debugger", "first");
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
    let sender = handle.input_sender(&id).unwrap();
    let pause = sender
        .try_control("pause".into(), InputControl::Pause)
        .unwrap();
    assert_eq!(
        timeout(WAIT, pause).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    let mut debugger = handle.target().attach_debugger().unwrap();
    timeout(WAIT, &mut debugger.ready).await.unwrap().unwrap();
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    debugger.send(InteractiveCommand::Quit).await.unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    let correction = sender
        .try_send("correction".into(), "Still paused.".into())
        .unwrap();
    assert_eq!(
        timeout(WAIT, correction).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    let resume = sender
        .try_control("resume".into(), InputControl::Resume)
        .unwrap();
    assert_eq!(
        timeout(WAIT, resume).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .chat_history
        .iter()
        .any(|m| m.as_ref() == &Message::user("[operator]: Still paused.")));
}

#[tokio::test]
async fn released_debugger_turn_cannot_bypass_successor_pause_on_inbox_resume() {
    for quit in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let registry = InputRegistry::default();
        let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
        handle.set_input_registry(registry.clone());
        let id = identity("inbox-resume-successor", "first");
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
        let sender = handle.input_sender(&id).unwrap();
        let pause = sender
            .try_control("pause".into(), InputControl::Pause)
            .unwrap();
        assert_eq!(
            timeout(WAIT, pause).await.unwrap().unwrap(),
            Delivery::Delivered
        );
        let mut first = handle.target().attach_debugger().unwrap();
        timeout(WAIT, &mut first.ready).await.unwrap().unwrap();
        first.send(InteractiveCommand::RunTurn).await.unwrap();
        // Let the turn grant reach the inbox gate before releasing its owner.
        assert!(timeout(QUIET, &mut agent).await.is_err());
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        if quit {
            first.send(InteractiveCommand::Quit).await.unwrap();
            timeout(WAIT, &mut first.ended).await.unwrap().unwrap();
        }
        drop(first);
        let mut successor = handle.target().attach_debugger().unwrap();
        timeout(WAIT, &mut successor.ready).await.unwrap().unwrap();
        let resume = sender
            .try_control("resume".into(), InputControl::Resume)
            .unwrap();
        assert_eq!(
            timeout(WAIT, resume).await.unwrap().unwrap(),
            Delivery::Delivered
        );
        assert!(timeout(QUIET, &mut agent).await.is_err());
        assert_eq!(transport.requests.lock().unwrap().len(), 1);

        successor.send(InteractiveCommand::RunTurn).await.unwrap();
        timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn correction_while_legacy_paused_preserves_pending_inbox_step() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("mixed-gates-pending-step", "first");
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
    let sender = handle.input_sender(&id).unwrap();
    let pause = sender
        .try_control("pause".into(), InputControl::Pause)
        .unwrap();
    assert_eq!(
        timeout(WAIT, pause).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    let mut debugger = handle.target().attach_debugger().unwrap();
    timeout(WAIT, &mut debugger.ready).await.unwrap().unwrap();
    let step = sender
        .try_control("step".into(), InputControl::Step)
        .unwrap();
    assert_eq!(
        timeout(WAIT, step).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    let correction = sender
        .try_send("correction".into(), "Preserve the granted step.".into())
        .unwrap();
    assert_eq!(
        timeout(WAIT, correction).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(!dir.path().join("stepped.txt").exists());

    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    timeout(WAIT, async {
        while !dir.path().join("stepped.txt").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1]
            .chat_history
            .iter()
            .any(|m| m.as_ref() == &Message::user("[operator]: Preserve the granted step.")));
    }
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
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    debugger.send(InteractiveCommand::Quit).await.unwrap();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn correction_after_legacy_step_does_not_authorize_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("legacy-step-late-correction", "first");
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
    let mut debugger = handle.target().attach_debugger().unwrap();
    timeout(WAIT, &mut debugger.ready).await.unwrap().unwrap();
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    timeout(WAIT, async {
        while !dir.path().join("stepped.txt").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    let sender = handle.input_sender(&id).unwrap();
    let correction = sender
        .try_send("late".into(), "For the next authorized turn.".into())
        .unwrap();
    assert_eq!(
        timeout(WAIT, correction).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(format!("{:?}", requests[2].chat_history)
        .contains("[operator]: For the next authorized turn."));
}

#[tokio::test]
async fn correction_during_legacy_step_tool_does_not_authorize_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("legacy-step-tool-correction", "first");
    let cancel = CancelToken::new();
    let session = session(&mut factory, &registry, &id, &cancel);
    let transport = Scripted {
        held: true,
        write_after_hold: true,
        held_tool: true,
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
    let mut debugger = handle.target().attach_debugger().unwrap();
    timeout(WAIT, &mut debugger.ready).await.unwrap().unwrap();
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    timeout(WAIT, async {
        while !dir.path().join("tool-entered").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    assert!(!dir.path().join("tool-finished").exists());
    let sender = handle.input_sender(&id).unwrap();
    let correction = sender
        .try_send(
            "during-tool".into(),
            "Correction while the tool is blocked.".into(),
        )
        .unwrap();
    std::fs::write(dir.path().join("tool-release"), "").unwrap();
    assert_eq!(
        timeout(WAIT, correction).await.unwrap().unwrap(),
        Delivery::Delivered
    );
    timeout(WAIT, async {
        while !dir.path().join("tool-finished").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(format!("{:?}", requests[2].chat_history)
        .contains("[operator]: Correction while the tool is blocked."));
}

#[tokio::test]
async fn two_legacy_run_turns_while_inbox_paused_survive_resume() {
    let dir = tempfile::tempdir().unwrap();
    let registry = InputRegistry::default();
    let (mut factory, handle) = InteractiveSessionFactory::for_pipeline();
    handle.set_input_registry(registry.clone());
    let id = identity("inbox-paused-legacy-grants", "first");
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
    let mut debugger = handle.target().attach_debugger().unwrap();
    timeout(WAIT, &mut debugger.ready).await.unwrap().unwrap();
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    assert!(timeout(QUIET, &mut agent).await.is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert!(!dir.path().join("stepped.txt").exists());

    let resume = sender
        .try_control("resume".into(), InputControl::Resume)
        .unwrap();
    assert_eq!(
        timeout(WAIT, resume).await.unwrap().unwrap(),
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
    assert_eq!(
        std::fs::read_to_string(dir.path().join("stepped.txt")).unwrap(),
        "one step"
    );

    // Inbox resume must not release the debugger after its two grants are spent.
    debugger.send(InteractiveCommand::RunTurn).await.unwrap();
    timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 4);
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
