use super::*;
use gremlins_magic_bus::{
    connection::BusSubGuard, subscribe_run, subscribe_run_with_history, DispatchEvent, EventKind,
    ReceiverHalf, Recv,
};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, TcpListener, UnixStream};

struct SessionChat {
    registry: TestRegistry,
    session: SessionTaskHandle,
    session_id: String,
    id: String,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl SessionChat {
    async fn new() -> Self {
        let (registry, _) = spawn_test_actor().await;
        let sessions = Default::default();
        let created =
            super::super::ops::handle_create_session(&json!({}), &registry, &sessions).await;
        assert_eq!(created["type"], "ok", "{created}");
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let session = sessions.lock().unwrap().get(&session_id).unwrap().clone();
        let chat_run_id = created["chat_run_id"].as_str().unwrap().to_string();
        assert!(!chat_run_id.is_empty(), "{created}");
        assert_eq!(session.get_chat_run_id().await, Some(chat_run_id.clone()));
        let path = crate::config::state_root().join(created["socket"].as_str().unwrap());
        let (reader, writer) = UnixStream::connect(path).await.unwrap().into_split();
        Self {
            registry,
            session,
            session_id,
            id: chat_run_id,
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn request(&mut self, mut request: Value) -> Value {
        request["session_id"] = json!(self.session_id);
        socket::write_json_line(&mut self.writer, &request)
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            socket::read_json_line(&mut self.reader),
        )
        .await
        .expect("request acknowledgement timed out")
        .unwrap()
        .expect("session_chat socket closed")
    }

    async fn prompt(&mut self, turn: &str, project: &std::path::Path) -> Value {
        let ack = self
            .request(json!({"op":"chat", "text":"Review draft", "turn":turn,
            "project_root":project, "req_id":turn,
            // Do not inherit the daemon's OnceLock snapshot: another test may
            // initialize it while exercising a deliberately broken PATH.
            "env":{"PATH":"/usr/bin:/bin"}}))
            .await;
        assert_eq!(ack["type"], "ok", "{ack}");
        assert!(!ack["chat_run_id"].as_str().unwrap().is_empty(), "{ack}");
        assert_eq!(ack["enqueued"], true, "{ack}");
        assert!(ack.get("id").is_none(), "{ack}");
        assert!(ack.get("stage_run_id").is_none(), "{ack}");
        assert!(ack.get("nonce").is_none(), "{ack}");
        ack
    }

    async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                socket::read_json_line(&mut self.reader)
            )
            .await
            .is_err(),
            "prompt connection received unsolicited output or closed"
        );
    }
}

fn provider_sandbox(provider: &TcpListener, model: &str) -> Sandbox {
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key":"test", "base-url":format!("http://{}", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    std::fs::write(
        sandbox.join("config/settings.yaml"),
        format!("default-model: openrouter/{model}\n"),
    )
    .unwrap();
    sandbox
}

async fn event_until(
    receiver: &mut ReceiverHalf<EventKind>,
    predicate: impl Fn(&EventKind) -> bool,
) -> DispatchEvent<EventKind> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Recv::Event(event) = receiver.recv().await else {
                panic!("bus closed or lagged")
            };
            if predicate(event.kind()) {
                return event;
            }
        }
    })
    .await
    .expect("session_chat bus event timed out")
}

async fn returned(session: &SessionTaskHandle) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if session.get_current_turn().await.is_none()
                && !session.get_chat_gremlin_keys().await.is_empty()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session_chat gremlin did not return");
}

#[tokio::test]
async fn reused_session_chat_turns_retain_actual_stage_identity_and_turn_correlation() {
    use super::super::test_provider::pending_request;
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = provider_sandbox(&provider, "test-session_chat-bus-reused");
    let mut session_chat = SessionChat::new().await;
    let (sub, mut events) = subscribe_run(&session_chat.id);
    let _guard = BusSubGuard::new(sub);
    let mut identities = Vec::new();
    for turn in ["first", "second"] {
        let ack = session_chat.prompt(turn, sandbox.path()).await;
        assert_eq!(ack["chat_run_id"], session_chat.id);
        let started = event_until(&mut events, |kind| {
            matches!(kind, EventKind::TurnStarted { .. })
        })
        .await;
        let EventKind::TurnStarted { stage_run_id } = started.kind() else {
            unreachable!()
        };
        assert_eq!(started.corr_id().run(), Some(session_chat.id.as_str()));
        assert!(
            started.corr_id().to_string().contains(turn),
            "missing prompt token: {:?}",
            started.corr_id()
        );
        assert_eq!(
            started.corr_id().stage_run_id(),
            Some(stage_run_id.as_str())
        );
        assert!(!stage_run_id.is_empty());
        let (_, held) = pending_request(&provider).await;
        let (interactive, _, _) = session_chat
            .registry
            .get_run_interactive(&session_chat.id)
            .await
            .unwrap();
        let (actual, _) = interactive
            .resolve_input(&session_chat.id, stage_run_id, None)
            .expect("announced stage must bind actual input");
        assert_eq!(actual.stage_run_id, *stage_run_id);
        assert_eq!(started.corr_id().stage(), Some(actual.stage_name.as_str()));
        identities.push(stage_run_id.clone());
        session_chat.quiet().await;
        let mode = if turn == "first" { "soft" } else { "hard" };
        let cancelled = session_chat
            .request(json!({"op":"cancel_chat", "turn":turn, "mode":mode}))
            .await;
        assert_eq!(cancelled["type"], "ok", "{cancelled}");
        let ended = event_until(&mut events, |kind| matches!(kind, EventKind::Ended { .. })).await;
        assert_eq!(ended.corr_id(), started.corr_id());
        drop(held);
        returned(&session_chat.session).await;
        session_chat.quiet().await;
    }
    assert_ne!(identities[0], identities[1]);
    let (sub, _, history) = subscribe_run_with_history(&session_chat.id, 1024);
    let _history_guard = BusSubGuard::new(sub);
    for (turn, identity) in ["first", "second"].into_iter().zip(&identities) {
        assert!(history.events.iter().any(|event| matches!(event.kind(), EventKind::TurnStarted { stage_run_id } if stage_run_id == identity)
            && event.corr_id().to_string().contains(turn)));
        assert!(history.events.iter().any(|event| matches!(event.kind(), EventKind::StageBoundary { stage_run_id, active: true, .. } if stage_run_id == identity)
            && event.corr_id().to_string().contains(turn)));
    }
    let old_id = session_chat.id.clone();
    let stopped = session_chat
        .request(json!({"op":"stop", "id":old_id}))
        .await;
    assert_eq!(stopped["type"], "ok", "{stopped}");
    assert!(session_chat
        .registry
        .get_run_interactive(&old_id)
        .await
        .is_none());
    let replacement = session_chat.prompt("replacement", sandbox.path()).await;
    assert_ne!(replacement["chat_run_id"], old_id);
    let replacement_id = replacement["chat_run_id"].as_str().unwrap().to_string();
    let (sub, mut replacement_events) = subscribe_run(&replacement_id);
    let _replacement_guard = BusSubGuard::new(sub);
    let (_, held) = pending_request(&provider).await;
    let current = session_chat.session.get_current_turn().await.unwrap();
    assert_eq!(current.id, replacement_id);
    assert!(session_chat
        .registry
        .get_run_interactive(&replacement_id)
        .await
        .is_some());
    session_chat.quiet().await; // The acknowledgement is not followed by a start frame.
    let cancelled = session_chat
        .request(json!({"op":"cancel_chat", "turn":"replacement", "mode":"soft"}))
        .await;
    assert_eq!(cancelled["type"], "ok", "{cancelled}");
    let ended = event_until(&mut replacement_events, |kind| {
        matches!(kind, EventKind::Ended { .. })
    })
    .await;
    assert_eq!(ended.corr_id().run(), Some(replacement_id.as_str()));
    assert!(ended.corr_id().to_string().contains("replacement"));
    drop(held);
    returned(&session_chat.session).await;
    teardown_session(&session_chat.session).await;
}

#[tokio::test]
async fn prompt_during_setup_keeps_the_reserved_session_chat() {
    let sandbox =
        Sandbox::with_config(Some("default-model: \"cmd/sh -c 'printf setup-result'\"\n"));
    let mut session_chat = SessionChat::new().await;
    let gate = session_chat.session.gate_turn("first-setup", "setup").await;
    let (sub, mut events) = subscribe_run(&session_chat.id);
    let _guard = BusSubGuard::new(sub);
    assert_eq!(
        session_chat.prompt("first-setup", sandbox.path()).await["chat_run_id"],
        session_chat.id
    );
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
    assert!(session_chat.session.get_current_turn().await.is_none());
    assert!(session_chat
        .session
        .get_chat_gremlin_keys()
        .await
        .is_empty());
    assert!(session_chat.session.get_chat_run_id().await.is_none());
    assert_eq!(
        session_chat.session.get_chat_handle().await.unwrap().id,
        session_chat.id
    );

    let second = session_chat.prompt("second-setup", sandbox.path()).await;
    assert_eq!(second["chat_run_id"], session_chat.id);
    assert!(session_chat.session.get_chat_run_id().await.is_none());
    assert_eq!(
        session_chat.session.get_chat_handle().await.unwrap().id,
        session_chat.id
    );
    gate.release.notify_one();
    for turn in ["first-setup", "second-setup"] {
        let done = event_until(&mut events, |kind| matches!(kind, EventKind::Done { .. })).await;
        assert_eq!(done.corr_id().run(), Some(session_chat.id.as_str()));
        assert!(done.corr_id().to_string().contains(turn));
    }
    returned(&session_chat.session).await;
    assert!(session_chat.session.get_chat_run_id().await.is_none());
    teardown_session(&session_chat.session).await;
}

#[tokio::test]
async fn stopped_queued_session_chat_prompt_ends_on_bus_without_stage_identity() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = provider_sandbox(&provider, "test-session_chat-bus-queued");
    let project = sandbox.path().to_path_buf();
    let mut session_chat = SessionChat::new().await;
    assert_eq!(
        session_chat.session.get_chat_run_id().await,
        Some(session_chat.id.clone())
    );
    let gate = session_chat.session.gate_turn("queued", "start").await;
    let (sub, mut events) = subscribe_run(&session_chat.id);
    let _guard = BusSubGuard::new(sub);
    let ack = session_chat.prompt("queued", &project).await;
    assert_eq!(ack["chat_run_id"], session_chat.id);
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
    assert!(session_chat.session.get_current_turn().await.is_none());
    assert!(session_chat.session.get_chat_run_id().await.is_none());
    let (interactive, _, _) = session_chat
        .registry
        .get_run_interactive(&session_chat.id)
        .await
        .unwrap();
    assert!(interactive.target().stage_run_id().is_none());
    assert!(!interactive.target().accepts_commands());
    assert!(interactive
        .resolve_input(&session_chat.id, "chat/0001", None)
        .is_none());
    let stopped = session_chat
        .request(json!({"op":"stop", "id":session_chat.id}))
        .await;
    assert_eq!(stopped["type"], "ok", "{stopped}");
    assert!(session_chat
        .registry
        .get_run_interactive(&session_chat.id)
        .await
        .is_none());
    assert!(interactive.target().stage_run_id().is_none());
    assert!(!interactive.target().accepts_commands());
    assert!(interactive
        .resolve_input(&session_chat.id, "chat/0001", None)
        .is_none());
    gate.release.notify_one();
    let ended = event_until(&mut events, |kind| matches!(kind, EventKind::Ended { .. })).await;
    let (sub, _, history) = subscribe_run_with_history(&session_chat.id, 1024);
    let _history_guard = BusSubGuard::new(sub);
    for event in &history.events {
        if event.corr_id().run() == Some(session_chat.id.as_str())
            && event.corr_id().to_string().contains("queued")
        {
            assert!(
                !matches!(
                    event.kind(),
                    EventKind::TurnStarted { .. } | EventKind::StageBoundary { .. }
                ),
                "queued prompt acquired stage identity: {event:?}"
            );
        }
    }
    assert_eq!(ended.corr_id().run(), Some(session_chat.id.as_str()));
    assert!(ended.corr_id().to_string().contains("queued"));
    assert!(ended.corr_id().stage_run_id().is_none());
    assert!(ended.corr_id().stage().is_none());
    session_chat.quiet().await;
    teardown_session(&session_chat.session).await;
}

#[tokio::test]
async fn preparation_failure_ends_identified_turn_without_orphan_input() {
    let sandbox = Sandbox::with_config(Some(
        "default-model: \"cmd/'unterminated-session_chat-bus\"\n",
    ));
    let mut session_chat = SessionChat::new().await;
    let (sub, mut events) = subscribe_run(&session_chat.id);
    let _guard = BusSubGuard::new(sub);
    session_chat.prompt("setup-failure", sandbox.path()).await;
    let error = event_until(&mut events, |kind| matches!(kind, EventKind::Error { .. })).await;
    let ended = event_until(&mut events, |kind| matches!(kind, EventKind::Ended { .. })).await;
    assert_eq!(error.corr_id(), ended.corr_id());
    assert_eq!(error.corr_id().run(), Some(session_chat.id.as_str()));
    assert!(error.corr_id().to_string().contains("setup-failure"));
    assert!(error.corr_id().stage_run_id().is_none());
    assert!(error.corr_id().stage().is_none());
    assert!(matches!(error.kind(), EventKind::Error { message, turn }
        if message.contains("failed to build chat backend") && turn.as_deref() == Some("setup-failure")));
    returned(&session_chat.session).await;
    let (interactive, _, _) = session_chat
        .registry
        .get_run_interactive(&session_chat.id)
        .await
        .unwrap();
    assert!(interactive.target().stage_run_id().is_none());
    assert!(!interactive.target().accepts_commands());
    assert!(interactive
        .resolve_input(&session_chat.id, "chat/0001", None)
        .is_none());
    let (sub, _, history) = subscribe_run_with_history(&session_chat.id, 1024);
    let _history_guard = BusSubGuard::new(sub);
    assert!(!history
        .events
        .iter()
        .any(|event| event.corr_id() == error.corr_id()
            && matches!(
                event.kind(),
                EventKind::TurnStarted { .. } | EventKind::StageBoundary { .. }
            )));
    session_chat.quiet().await;
    teardown_session(&session_chat.session).await;
}

#[tokio::test]
async fn command_session_chat_publishes_one_done_with_actual_output_and_can_be_reused() {
    let sandbox = Sandbox::with_config(Some(
        "default-model: \"cmd/sh -c 'printf command-session_chat-output'\"\n",
    ));
    let mut session_chat = SessionChat::new().await;
    let (sub, mut events) = subscribe_run(&session_chat.id);
    let _guard = BusSubGuard::new(sub);
    let mut identities = Vec::new();
    for turn in ["command-first", "command-reused"] {
        assert_eq!(
            session_chat.prompt(turn, sandbox.path()).await["chat_run_id"],
            session_chat.id
        );
        let started = event_until(&mut events, |kind| {
            matches!(kind, EventKind::TurnStarted { .. })
        })
        .await;
        let done = event_until(&mut events, |kind| matches!(kind, EventKind::Done { .. })).await;
        assert_eq!(done.corr_id(), started.corr_id());
        assert!(done.corr_id().to_string().contains(turn));
        identities.push(done.corr_id().stage_run_id().unwrap().to_string());
        let EventKind::Done { text, summary, .. } = done.kind() else {
            unreachable!()
        };
        assert_eq!(text, "command-session_chat-output");
        assert!(
            summary.is_empty(),
            "command text must not be duplicated in summary"
        );
        returned(&session_chat.session).await;
        let (sub, _, history) = subscribe_run_with_history(&session_chat.id, 1024);
        let _history_guard = BusSubGuard::new(sub);
        assert_eq!(
            history
                .events
                .iter()
                .filter(|event| event.corr_id() == done.corr_id()
                    && matches!(event.kind(), EventKind::Done { .. }))
                .count(),
            1
        );
        assert!(!history
            .events
            .iter()
            .any(|event| event.corr_id() == done.corr_id()
                && matches!(event.kind(), EventKind::Error { .. })));
        session_chat.quiet().await;
    }
    assert_ne!(identities[0], identities[1]);
    teardown_session(&session_chat.session).await;
}

async fn complete_provider(stream: &mut tokio::net::TcpStream, tool: bool) {
    let chunks = if tool {
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"bash-session_chat","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"printf session_chat-tool-output\"}"}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}),
        ]
    } else {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":"Draft reviewed. "},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"content":"No changes needed."},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"done-session_chat","type":"function","function":{"name":"Done","arguments":"{\"summary\":\"Review complete\"}"}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":7,"total_tokens":27}}),
        ]
    };
    let body = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_done_is_retained_with_cumulative_text_while_prompt_reader_stalls() {
    use super::super::test_provider::pending_request;
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = provider_sandbox(&provider, "test-session_chat-bus-done");
    let mut session_chat = SessionChat::new().await;
    let (sub, mut events) = subscribe_run(&session_chat.id);
    let _guard = BusSubGuard::new(sub);
    for (invocation, turn) in ["done-first", "done-reused"].into_iter().enumerate() {
        let ack = session_chat.prompt(turn, sandbox.path()).await;
        assert_eq!(ack["chat_run_id"], session_chat.id);
        let started = event_until(&mut events, |kind| {
            matches!(kind, EventKind::TurnStarted { .. })
        })
        .await;
        let (request, mut stream) = pending_request(&provider).await;
        assert!(request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message.to_string().contains("Review draft")));
        if turn == "done-first" {
            gremlins_magic_bus::emit_corr_event(
                started.corr_id(),
                EventKind::SkillLoaded {
                    id: "session_chat-bus-skill".into(),
                    name: "retained-session_chat-skill".into(),
                },
            );
        }
        let mut completion = session_chat.session.get_current_turn().await.unwrap().done;
        assert!(!*completion.borrow(), "provider is still held");
        complete_provider(&mut stream, true).await;
        let (request, mut stream) = pending_request(&provider).await;
        let tool_messages: Vec<_> = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert!(
            tool_messages
                .iter()
                .any(|message| message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("session_chat-tool-output")
                        && !text.contains("Error:"))),
            "expected successful Bash output; tool messages: {tool_messages:?}"
        );
        complete_provider(&mut stream, false).await;
        // Do not read the prompt socket until the bus and session have settled.
        let done = event_until(&mut events, |kind| matches!(kind, EventKind::Done { .. })).await;
        assert_eq!(done.corr_id(), started.corr_id());
        assert!(done.corr_id().to_string().contains(turn));
        let EventKind::Done {
            text,
            summary,
            usage,
        } = done.kind()
        else {
            unreachable!()
        };
        assert_eq!(text, "Draft reviewed. No changes needed.");
        assert_eq!(summary, "Review complete");
        let usage = usage.as_ref().expect("Done must retain cumulative usage");
        let count = invocation as u64 + 1;
        assert_eq!(usage.prompt_tokens, Some(30 * count));
        assert_eq!(usage.completion_tokens, Some(10 * count));
        assert_eq!(usage.turns, 2 * (invocation + 1));
        tokio::time::timeout(Duration::from_secs(10), completion.wait_for(|done| *done))
            .await
            .expect("unread prompt display blocked turn completion")
            .expect("completion sender dropped before completion");
        returned(&session_chat.session).await;
        assert_eq!(
            session_chat.session.get_combined_skills().await.unwrap(),
            vec!["retained-session_chat-skill"]
        );
        session_chat.quiet().await;
        let (sub, _, history) = subscribe_run_with_history(&session_chat.id, 1024);
        let _history_guard = BusSubGuard::new(sub);
        assert_eq!(
            history
                .events
                .iter()
                .filter(|event| event.corr_id() == done.corr_id()
                    && matches!(event.kind(), EventKind::Done { .. }))
                .count(),
            1,
            "exactly one Done per invocation"
        );
        assert!(history.events.iter().any(|event|
            event.corr_id().run() == done.corr_id().run()
            && event.corr_id().stage_run_id() == done.corr_id().stage_run_id()
            && matches!(event.kind(), EventKind::ToolOutputChunk { text, .. } if text.contains("session_chat-tool-output"))));
        assert!(history
            .events
            .iter()
            .any(|event| event.sequence() == done.sequence()
                && matches!(event.kind(), EventKind::Done { .. })
                && event.corr_id() == done.corr_id()));
        assert!(history.events.iter().any(|event| matches!(event.kind(), EventKind::StreamChunk { text } if text == "Draft reviewed. ")
            && event.corr_id() == done.corr_id()));
    }
    teardown_session(&session_chat.session).await;
}
