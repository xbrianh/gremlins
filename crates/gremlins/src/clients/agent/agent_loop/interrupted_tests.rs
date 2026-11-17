use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop, test_ctx, RunContext};
use crate::clients::cancel::CancelToken;
use crate::clients::interactive::{
    InteractiveCommand, InteractiveSession, InteractiveSessionFactory,
};
use crate::conversation::new_blank_conversation;
use futures::{stream, StreamExt};
use gremlins_magic_bus::{connection::BusSubGuard, subscribe_run, EventKind, Recv};
use rig_core::completion::message::{AssistantContent, ReasoningContent, ToolCall, ToolFunction};
use rig_core::completion::{CompletionRequest, Message};
use rig_core::driver::{Exchange, Model, Opened, Opening, Transport};
use rig_core::message::{Issuer, ToolName};
use rig_core::test_utils::{MockCompletionModel, MockFrame, MockScript, MockStreamEvent as Event};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

const PROMPT: &str = "Describe the old implementation.";
const FIRST: &str = "  First block. ";
const SECOND: &str = "Second block.\n";
const WAIT: Duration = Duration::from_secs(5);

fn two_blocks() -> Vec<Event> {
    vec![
        Event::text(FIRST),
        Event::text_start("second", None), // closes the first block
        Event::text(SECOND),               // delta only until the next block or EOF
    ]
}

fn finish() -> Vec<Event> {
    vec![
        Event::tool_call("finish", "Done", json!({"summary": "finished"})),
        Event::final_response_with_default_usage(),
    ]
}

/// Preserve a previous, answered tool call in every test: committing or rolling
/// back the current prompt must never remove the preceding result.
fn earlier_messages() -> Vec<Message> {
    let read = ToolCall::from_wire(
        "prior-read",
        ToolFunction::new(ToolName::new("Read").unwrap(), json!({"file_path": "old"})),
    );
    vec![
        Message::user("Read the old file."),
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(read.clone())],
        },
        Message::tool_result(read.id, read.function.name, "old contents"),
    ]
}

fn standard_ctx(cwd: &Path, id: &str) -> RunContext {
    let mut ctx = test_ctx(Some(cwd.to_path_buf()), None);
    ctx.idle_timeout = 30.0; // only the operator, never the idle timeout, ends the held stream
    ctx.params.idle_timeout = Some(ctx.idle_timeout);
    ctx.params.gremlin_id = Some(id.to_owned());
    ctx.prepopulated_conversation = new_blank_conversation(PROMPT, false, Some(earlier_messages()));
    ctx
}

/// The first response yields the script, then remains pending forever. Polling
/// the tail means the transport is drained, not necessarily that its deltas
/// reached subscribers. The next request completes normally.
#[derive(Clone)]
struct HeldResponse {
    prefix: Vec<Event>,
    stalled: Arc<Notify>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Transport<MockScript> for HeldResponse {
    fn send(&self, request: CompletionRequest, _: Exchange) -> Opening<MockFrame> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        match requests.len() {
            1 => {
                let frames = stream::iter(self.prefix.clone())
                    .map(MockFrame::Event)
                    .map(Ok);
                let stalled = self.stalled.clone();
                Opening::ready(Opened::new(frames.chain(stream::once(async move {
                    stalled.notify_one();
                    std::future::pending().await
                }))))
            }
            2 => Opening::ready(Opened::new(
                stream::iter(finish()).map(MockFrame::Event).map(Ok),
            )),
            n => panic!("unexpected model request #{n}"),
        }
    }
}

/// Return the non-system messages *sent to the provider* after the operator's
/// command. System instructions change while debugging; the earlier transcript
/// must not. The new request's suffix contains just the turn under test.
async fn pause_and_replay(
    id: &str,
    prefix: Vec<Event>,
    visible: &str,
    preserve_reasoning: bool,
    command: InteractiveCommand,
) -> Vec<Message> {
    let dir = tempfile::tempdir().unwrap();
    let ctx = standard_ctx(dir.path(), id);
    let mut opts = loop_opts(None);
    opts.preserve_reasoning_in_history = preserve_reasoning;
    let transport = HeldResponse {
        prefix,
        stalled: Arc::new(Notify::new()),
        requests: Arc::default(),
    };
    let model = Model::new(MockScript::default(), transport.clone()).erase();
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    let agent = tokio::spawn(run_agent_loop(
        model,
        PROMPT,
        ctx,
        CancelToken::new(),
        opts,
        session,
    ));

    let mut attachment = None;
    if visible.is_empty() {
        // Without a text chunk to observe, wait until the transport stalls.
        timeout(WAIT, transport.stalled.notified())
            .await
            .expect("first stream stalled");
        attachment = Some(handle.attach().unwrap());
    }
    timeout(WAIT, async {
        let mut streamed = String::new();
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("lost run events")
            };
            match event.kind() {
                EventKind::StreamChunk { text } => {
                    streamed.push_str(text);
                    assert!(
                        visible.starts_with(&streamed),
                        "unexpected text: {streamed:?}"
                    );
                    if !text.is_empty() && streamed == visible {
                        attachment = Some(handle.attach().unwrap());
                    }
                }
                EventKind::Ready { turn } => {
                    assert_eq!(*turn, 0);
                    assert_eq!(streamed, visible);
                    break;
                }
                EventKind::TurnComplete { .. } => panic!("pause completed an unfinished turn"),
                EventKind::ToolResult { .. } => panic!("dispatched an interrupted tool"),
                _ => {}
            }
        }
    })
    .await
    .expect("expected visible chunks followed by Ready");
    let commands = attachment.expect("Ready must follow attachment");
    commands.send(command).await.unwrap();
    let run = timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(run.text_result.as_deref(), Some("finished"));
    assert_eq!(run.token_usage.unwrap().turns, 1);
    assert!(
        !dir.path().join("must-not-exist").exists(),
        "dispatched an interrupted Write"
    );

    let mut requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let mut replay = requests.pop().unwrap().chat_history;
    replay.retain(|m| !matches!(m, Message::System { .. }));
    let earlier = earlier_messages();
    assert!(
        replay.starts_with(&earlier),
        "lost earlier tool results: {replay:?}"
    );
    replay.split_off(earlier.len())
}

#[tokio::test]
async fn injected_correction_follows_completed_block_and_visible_delta_once() {
    let mut prefix = vec![
        Event::tool_call(
            "abandoned",
            "Write",
            json!({"file_path":"must-not-exist", "content":"bad"}),
        ),
        Event::reasoning("not replayed alongside prose"),
    ];
    prefix.extend(two_blocks());
    let partial = format!("{FIRST}{SECOND}");
    let replay = pause_and_replay(
        "history-inject-two-blocks",
        prefix,
        &partial,
        false,
        InteractiveCommand::Inject("Actually, describe the new one.".into()),
    )
    .await;
    assert_eq!(
        replay,
        [
            Message::user(PROMPT),
            Message::assistant(partial),
            Message::user("[operator]: Actually, describe the new one."),
        ]
    );
}

#[tokio::test]
async fn resume_retains_partial_prose_and_only_sealed_reasoning() {
    let replay = pause_and_replay(
        "history-resume-reasoning",
        vec![
            Event::reasoning("verified").with_reasoning_id("completed-item"),
            Event::reasoning_delta_with_id("unfinished-item", "never sealed"),
            Event::text("partial prose"),
        ],
        "partial prose",
        true,
        InteractiveCommand::Quit,
    )
    .await;
    let [original, Message::Assistant { content, .. }, continuation] = replay.as_slice() else {
        panic!("unexpected resumed request: {replay:?}");
    };
    assert_eq!(original, &Message::user(PROMPT));
    assert_eq!(continuation, &Message::user("Continue."));
    let [AssistantContent::Reasoning(item), AssistantContent::Text(prose)] = content.as_slice()
    else {
        panic!("expected exactly one completed reasoning item and prose: {content:?}");
    };
    let item = item
        .open(&Issuer::from("mock"))
        .expect("original provider seal");
    assert_eq!(item.id.as_deref(), Some("completed-item"));
    assert!(
        matches!(&item.content[..], [ReasoningContent::Text { text, .. }] if text == "verified")
    );
    assert_eq!(prose.text, "partial prose");
}

#[tokio::test]
async fn pause_before_output_preserves_pending_prompt() {
    let replay =
        pause_and_replay("history-empty", vec![], "", false, InteractiveCommand::Quit).await;
    assert_eq!(replay, [Message::user(PROMPT)]);
}

#[tokio::test]
async fn whitespace_only_pause_preserves_pending_prompt() {
    let replay = pause_and_replay(
        "history-whitespace",
        vec![Event::text("\n")],
        "\n",
        false,
        InteractiveCommand::Quit,
    )
    .await;
    assert_eq!(replay, [Message::user(PROMPT)]);
}

#[tokio::test]
async fn undispatched_call_and_display_only_reasoning_have_no_history() {
    let replay = pause_and_replay(
        "history-unexecuted",
        vec![
            Event::tool_call_name_delta("write", "Write"),
            Event::tool_call_arguments_delta(
                "write",
                r#"{"file_path":"must-not-exist","content":"bad"}"#,
            ),
            Event::tool_call_end("write"),
            Event::reasoning_delta("still thinking"),
        ],
        "",
        true,
        InteractiveCommand::Quit,
    )
    .await;
    assert_eq!(replay, [Message::user(PROMPT)]);
}

async fn cancel_after_step(id: &str, queue_command: bool) {
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([[
        Event::text("Here is one step."),
        Event::final_response_with_default_usage(),
    ]]);
    let cancel = CancelToken::new();
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();
    let commands = handle.attach().unwrap();
    let agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        PROMPT,
        standard_ctx(dir.path(), id),
        cancel.clone(),
        loop_opts(None),
        session,
    ));
    for expected_turn in 0..=1 {
        let turn = timeout(WAIT, async {
            loop {
                let Recv::Event(event) = events.recv().await else {
                    panic!("lost run events");
                };
                if let EventKind::Ready { turn } = event.kind() {
                    break *turn;
                }
            }
        })
        .await
        .expect("expected Ready at step boundary");
        assert_eq!(turn, expected_turn);
        if turn == 0 {
            commands.send(InteractiveCommand::RunTurn).await.unwrap();
        }
    }
    // On this single-threaded runtime neither wake can run the agent until we yield.
    if queue_command {
        commands
            .send(InteractiveCommand::Bail("stale".into()))
            .await
            .unwrap();
    }
    cancel.cancel();
    let result = timeout(WAIT, agent)
        .await
        .expect("step remained paused")
        .unwrap();
    assert!(matches!(
        result,
        Err(crate::clients::error::ClientError::Cancelled)
    ));
    assert_eq!(model.request_count(), 1);
}

#[tokio::test]
async fn cancellation_wakes_wait_after_debug_step() {
    cancel_after_step("cancel-after-step", false).await;
}

#[tokio::test]
async fn cancellation_wins_over_a_ready_debug_command() {
    cancel_after_step("cancel-ready-debug-command", true).await;
}

#[tokio::test]
async fn uninterrupted_text_blocks_are_finalized_once_without_ready() {
    let id = "history-uninterrupted";
    let dir = tempfile::tempdir().unwrap();
    let mut events = two_blocks();
    events.extend(finish());
    let model = MockCompletionModel::from_stream_turns([events]);
    let (sub, mut rx) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    let run = timeout(
        WAIT,
        run_agent_loop(
            model.clone().erase(),
            PROMPT,
            standard_ctx(dir.path(), id),
            CancelToken::new(),
            loop_opts(None),
            InteractiveSession::unattached(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(model.request_count(), 1);
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event.kind(), EventKind::Ready { .. }));
    }
    let messages = run.messages().unwrap();
    let [prompt, Message::Assistant { content, .. }, _done_result] =
        &messages[earlier_messages().len()..]
    else {
        panic!("unexpected finalized transcript: {messages:?}");
    };
    assert_eq!(prompt, &Message::user(PROMPT));
    let [AssistantContent::Text(text), AssistantContent::ToolCall(_done)] = content.as_slice()
    else {
        panic!("unexpected completed response: {content:?}");
    };
    assert_eq!(text.text, format!("{FIRST}{SECOND}"));
}

// ── Message (correction + release) ────────────────────────────────────────

#[tokio::test]
async fn message_correction_injects_text_and_releases_slot_so_agent_resumes() {
    let id = "history-message-correction";
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            // First turn: streams partial text, then pauses.
            Event::text("First partial. "),
        ],
        vec![
            // Second turn (after Message): processes correction autonomously.
            Event::text("Corrected output."),
            Event::tool_call("done1", "Done", json!({"summary": "corrected"})),
            Event::final_response_with_default_usage(),
        ],
    ]);
    let cancel = CancelToken::new();
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();
    let commands = handle.attach().unwrap();
    let agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        PROMPT,
        standard_ctx(dir.path(), id),
        cancel.clone(),
        loop_opts(None),
        session,
    ));

    // Wait for Ready after the first stream is interrupted.
    timeout(WAIT, async {
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("lost run events");
            };
            if let EventKind::Ready { .. } = event.kind() {
                break;
            }
        }
    })
    .await
    .expect("expected Ready after pause");

    // Send Message correction — should inject, release slot, and agent resumes.
    commands
        .send(InteractiveCommand::Message("fix this up".into()))
        .await
        .unwrap();

    // Agent should complete normally without another Ready.
    let run = timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(run.text_result.as_deref(), Some("Corrected output."));
    assert_eq!(model.requests().len(), 2);

    // Verify the second request carries the correction text exactly once.
    let corrected = &model.requests()[1];
    let mut correction_count = 0usize;
    for m in &corrected.chat_history {
        if let Message::User { content } = m {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    rig_core::completion::message::UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            if text.contains("[operator]: fix this up") {
                correction_count += 1;
            }
        }
    }
    assert_eq!(correction_count, 1, "correction must appear exactly once");
}

#[tokio::test]
async fn message_does_not_amend_system_prompt() {
    let id = "history-message-no-debug-note";
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![Event::text("Some. ")],
        vec![
            Event::text("Done."),
            Event::tool_call("d1", "Done", json!({"summary":"ok"})),
            Event::final_response_with_default_usage(),
        ],
    ]);
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();
    let commands = handle.attach().unwrap();
    let agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        PROMPT,
        standard_ctx(dir.path(), id),
        CancelToken::new(),
        loop_opts(None),
        session,
    ));

    // Wait for Ready.
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    timeout(WAIT, async {
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("lost run events");
            };
            if let EventKind::Ready { .. } = event.kind() {
                break;
            }
        }
    })
    .await
    .expect("expected Ready");

    commands
        .send(InteractiveCommand::Message("no debug prompt".into()))
        .await
        .unwrap();

    let run = timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(model.requests().len(), 2);

    // System prompt in the second request should NOT contain the debug note.
    let first = &model.requests()[0];
    let second = &model.requests()[1];
    let has_debug_note = second.chat_history.iter().any(|m| {
        if let Message::System { content } = m {
            content.contains("operator has connected in debug mode")
                || content.contains("Treat them as")
        } else {
            false
        }
    });
    assert!(
        !has_debug_note,
        "Message operator should not amend system prompt"
    );
    // The original system prompt must be preserved (identical to first request).
    let first_system: Vec<&str> = first
        .chat_history
        .iter()
        .filter_map(|m| {
            if let Message::System { content } = m {
                Some(content.as_str())
            } else {
                None
            }
        })
        .collect();
    let second_system: Vec<&str> = second
        .chat_history
        .iter()
        .filter_map(|m| {
            if let Message::System { content } = m {
                Some(content.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        first_system, second_system,
        "system prompt must be identical across requests"
    );
    drop(run);
}

#[tokio::test]
async fn message_releases_slot_so_second_debugger_can_attach() {
    let id = "history-message-release-slot";
    let dir = tempfile::tempdir().unwrap();
    // Turn 0: text (will pause). Turn 1: Done to finish.
    let model = MockCompletionModel::from_stream_turns([
        vec![Event::text("First batch. ")],
        vec![
            Event::text("Second batch."),
            Event::tool_call("d1", "Done", json!({"summary":"finished"})),
            Event::final_response_with_default_usage(),
        ],
    ]);
    let cancel = CancelToken::new();
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();

    let first = handle.attach().unwrap();
    let agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        PROMPT,
        standard_ctx(dir.path(), id),
        cancel.clone(),
        loop_opts(None),
        session,
    ));

    // Wait for first Ready.
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    timeout(WAIT, async {
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("lost run events");
            };
            if let EventKind::Ready { .. } = event.kind() {
                break;
            }
        }
    })
    .await
    .expect("expected first Ready");

    first
        .send(InteractiveCommand::Message("correction one".into()))
        .await
        .unwrap();
    // Dropping the first attachment releases the pause slot to Idle.
    drop(first);

    // After Message, the slot must be idle — a second debugger can
    // claim it. Drop the second attachment immediately so the agent
    // won't consume it; the attach succeeding is the actual test.
    assert!(
        handle.attach().is_some(),
        "slot must be idle after Message releases it"
    );

    let run = timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(run.text_result.as_deref(), Some("Second batch."));
    assert_eq!(model.requests().len(), 2);
}

#[tokio::test]
async fn message_correction_with_nonterminal_tool_requires_second_iteration() {
    let id = "history-message-multi-turn";
    let dir = tempfile::tempdir().unwrap();
    // Turn 0: partial text → pause.
    // Turn 1 (after Message): non-terminal tool + final_response → tool dispatched, loop continues.
    // Turn 2: agent finishes autonomously.
    let model = MockCompletionModel::from_stream_turns([
        vec![Event::text("Partial text. ")],
        vec![
            Event::text("Corrected text. "),
            Event::tool_call(
                "w1",
                "Write",
                json!({"file_path": dir.path().join("multi.txt").to_str().unwrap(), "content": "data"}),
            ),
            Event::final_response_with_default_usage(),
        ],
        vec![
            Event::text("All done."),
            Event::tool_call("d1", "Done", json!({"summary":"ok"})),
            Event::final_response_with_default_usage(),
        ],
    ]);
    let cancel = CancelToken::new();
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();
    let commands = handle.attach().unwrap();
    let agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        PROMPT,
        standard_ctx(dir.path(), id),
        cancel.clone(),
        loop_opts(None),
        session,
    ));

    // Wait for Ready.
    timeout(WAIT, async {
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("lost run events");
            };
            if let EventKind::Ready { .. } = event.kind() {
                break;
            }
        }
    })
    .await
    .expect("expected Ready");

    commands
        .send(InteractiveCommand::Message("fix it".into()))
        .await
        .unwrap();

    let run = timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(run.text_result.as_deref(), Some("All done."));
    // Three requests: original (partial), correction + Write, final Done.
    assert_eq!(model.requests().len(), 3);
    // No extra Ready events after the initial one.
    drop(events);
}

#[tokio::test]
async fn explicit_continue_still_amends_system_prompt() {
    let id = "history-continue-amends-prompt";
    let dir = tempfile::tempdir().unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![Event::text("Partial. ")],
        vec![
            Event::text("Stepped."),
            Event::tool_call("d1", "Done", json!({"summary":"ok"})),
            Event::final_response_with_default_usage(),
        ],
    ]);
    let (mut sessions, handle) = InteractiveSessionFactory::for_chat();
    let session = sessions.next_session();
    let commands = handle.attach().unwrap();
    let agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        PROMPT,
        standard_ctx(dir.path(), id),
        CancelToken::new(),
        loop_opts(None),
        session,
    ));
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    timeout(WAIT, async {
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("lost run events");
            };
            if let EventKind::Ready { .. } = event.kind() {
                break;
            }
        }
    })
    .await
    .expect("expected Ready");

    // Explicit step (RunTurn/"continue") should amend the system prompt.
    commands.send(InteractiveCommand::RunTurn).await.unwrap();

    let run = timeout(WAIT, agent).await.unwrap().unwrap().unwrap();
    assert_eq!(model.requests().len(), 2);
    let second = &model.requests()[1];
    let has_debug_note = second.chat_history.iter().any(|m| {
        if let Message::System { content } = m {
            content.contains("operator has connected in debug mode")
        } else {
            false
        }
    });
    assert!(has_debug_note, "RunTurn should amend the system prompt");
    drop(run);
}
