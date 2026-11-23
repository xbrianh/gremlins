use super::*;
use ratatui::backend::TestBackend;
use serde_json::{json, Value};
use tokio::net::UnixStream;

struct Fixture {
    app: App,
    live: LiveArea<TestBackend>,
    client: Arc<client::DaemonClient>,
    peers: [UnixStream; 2],
    attachment: Attachment,
    tx: mpsc::UnboundedSender<AttachmentFrame>,
    rx: mpsc::UnboundedReceiver<AttachmentFrame>,
    last_stage: Option<Target>,
    follow: Option<tokio::task::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let (client, peers) = client::DaemonClient::fake_with_identity(
            json!({"session_id":"test", "home_run_id":"home"}),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            app: App::new(),
            live: LiveArea::with_backend(TestBackend::new(80, 24), 80, 24),
            client: Arc::new(client),
            peers,
            attachment: Attachment::default(),
            tx,
            rx,
            last_stage: None,
            follow: None,
        }
    }

    fn watch(&mut self, id: &str) {
        self.app.watch_selection = Some(id.into());
        self.app.screen = Screen::Runs;
        self.live.show_runs();
        watch_enter(
            &mut self.app,
            &mut self.live,
            &self.client,
            &mut self.attachment,
            &self.tx,
            &mut self.last_stage,
            &mut self.follow,
        );
    }

    fn debug(&mut self, id: &str) {
        debug_entry(
            &mut self.app,
            &mut self.live,
            &self.client,
            id.into(),
            &mut self.attachment,
            &self.tx,
            &mut self.last_stage,
            &mut self.follow,
        );
    }

    fn control(&mut self, text: &str) -> bool {
        self.app.replace_input(text);
        selected_control(&mut self.app, &mut self.live, &mut self.attachment)
    }

    async fn frame(&mut self) {
        let frame = tokio::time::timeout(Duration::from_secs(2), self.rx.recv())
            .await
            .expect("attachment frame timed out")
            .unwrap();
        apply_attachment_frame(&mut self.app, &mut self.live, &mut self.attachment, frame);
    }

    async fn attach_watch(
        &mut self,
        listener: &tokio::net::UnixListener,
        execution: &str,
    ) -> BufReader<UnixStream> {
        self.app
            .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
        self.watch("run");
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut peer = BufReader::new(stream);
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"attach", "scope":"conversation", "id":"run", "stage":"review", "session_id":"test", "mode":"control"})
        );
        self.receive(&mut peer, json!({"type":"attached", "scope":"conversation", "id":"run", "stage":"review", "execution_id":execution, "nonce":9, "capabilities":["message"]})).await;
        peer
    }

    async fn attach_debug(&mut self, listener: &tokio::net::UnixListener) -> BufReader<UnixStream> {
        self.app
            .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
        self.debug("run");
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut peer = BufReader::new(stream);
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"attach", "scope":"conversation", "id":"run", "stage":"review", "session_id":"test", "mode":"debugger"})
        );
        self.receive(&mut peer, json!({"type":"attached", "scope":"conversation", "id":"run", "stage":"review", "execution_id":"exec", "nonce":9, "capabilities":["message"]})).await;
        assert_eq!(self.app.screen, Screen::Chat);
        assert_eq!(
            self.app.selected_target,
            Target::StageExecution {
                run_id: "run".into(),
                stage: "review".into()
            }
        );
        peer
    }

    async fn receive(&mut self, peer: &mut BufReader<UnixStream>, value: Value) {
        socket::write_json_line(peer.get_mut(), &value)
            .await
            .unwrap();
        self.frame().await;
    }

    fn rendered(&mut self) -> String {
        self.live.compose(&self.app).unwrap();
        let backend = self.live.backend();
        [backend.scrollback(), backend.buffer()]
            .into_iter()
            .flat_map(|buffer| {
                let area = buffer.area;
                (area.y..area.bottom()).map(move |y| {
                    (area.x..area.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_unsent(&self) {
        for peer in &self.peers {
            let mut byte = [0];
            assert_eq!(
                peer.try_read(&mut byte).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }
}

#[tokio::test]
async fn inline_watch_switch_away_back_rejects_buffered_output_for_same_run() {
    let mut f = Fixture::new();
    f.app.show_runs();
    f.live.show_runs();
    for id in ["run-a", "run-b"] {
        f.app
            .upsert_run_from_ls(&json!({"id":id, "status":"running"}));
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut generation = 0;
    let mut old_generations = Vec::new();
    for id in ["run-a", "run-b", "run-a"] {
        f.app.watch_selection = Some(id.into());
        start_watch_follow(
            &mut f.app,
            &mut f.live,
            &f.client,
            id.into(),
            &tx,
            &mut f.follow,
            &mut generation,
        );
        assert!(old_generations.iter().all(|old| *old != generation));
        old_generations.push(generation);
        let line = json!({"type":"view_log", "id":id, "generation":generation,
            "line":format!("visible-{generation}\n")});
        apply_watch_frame(&mut f.app, &mut f.live, &line, generation, &mut f.follow);
        let rendered = f.rendered();
        assert!(
            rendered.contains(&format!("visible-{generation}")),
            "{rendered}"
        );
        for old in &old_generations[..old_generations.len() - 1] {
            assert!(!rendered.contains(&format!("visible-{old}")), "{rendered}");
        }
        if old_generations.len() < 3 {
            tx.send(json!({"type":"view_log", "id":id, "generation":generation,
                "line":"stale-buffered-output\n"}))
                .unwrap();
            tx.send(
                json!({"type":"view_error", "id":id, "generation":generation,
                "message":"stale-buffered-error"}),
            )
            .unwrap();
        }
    }
    // Drain only after A -> B -> A: run-id filtering alone would accept old A.
    while let Ok(frame) = rx.try_recv() {
        apply_watch_frame(&mut f.app, &mut f.live, &frame, generation, &mut f.follow);
    }
    let rendered = f.rendered();
    assert!(rendered.contains("visible-3"), "{rendered}");
    assert!(!rendered.contains("stale-buffered"), "{rendered}");
    assert_eq!(f.app.watch_follow_error, None);
    assert!(
        f.follow.is_some(),
        "stale errors must not abort the new follow"
    );
    close_watch_follow(&mut f.app, &mut f.live, &mut f.follow);
    f.assert_unsent();
}

#[tokio::test]
async fn watch_home_preserves_idle_draft_without_attaching_or_sending() {
    let mut f = Fixture::new();
    install_home_session(&mut f.app, &f.client).await;
    f.app.replace_input("unsent home draft");
    f.watch("home");
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.screen, Screen::Chat);
    assert_eq!(f.app.input.text(), "unsent home draft");
    assert!(!f.app.active_request);
    assert!(f.attachment.target.is_none());
    assert!(f.attachment.task.is_none());
    assert!(f.attachment.commands.is_none());
    assert_eq!(
        f.app
            .conversations
            .submit(&Target::Home, &f.app.input, false),
        Submission::HomePrompt
    );
    assert!(!f.control("/continue"));
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn watch_home_keeps_the_same_stream_owner_and_one_response() {
    let mut f = Fixture::new();
    install_home_session(&mut f.app, &f.client).await;
    let owner = protocol::Execution {
        run_id: "home".into(),
        execution_id: "owner".into(),
        nonce: 7,
    };
    f.app
        .conversations
        .set_execution(Target::Home, Some(owner.clone()));
    f.app.active_request = true;
    f.live.submit_prompt("question".into());
    f.live.push_response("owner response");
    f.app.replace_input("owner correction");
    for _ in 0..2 {
        f.watch("home");
        assert_eq!(f.app.selected_target, Target::Home);
        assert!(f.app.active_request);
        assert_eq!(f.live.response_text(), "owner response");
        assert_eq!(f.app.input.text(), "owner correction");
        assert!(f.attachment.target.is_none());
        assert!(f.attachment.task.is_none());
    }
    assert!(
        matches!(f.app.conversations.submit(&Target::Home, &f.app.input, true),
        Submission::Correction { execution, .. } if execution == owner)
    );
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn session_install_uses_authoritative_identity_before_turn_started() {
    let mut f = Fixture::new();
    install_home_session(&mut f.app, &f.client).await;
    assert_eq!(f.app.home_run_id.as_deref(), Some("home"));
    f.app.conversations.begin_home_turn("pending-turn".into());
    f.app.active_request = true;
    f.app
        .upsert_run_from_ls(&json!({"id":"home", "stage":"chat", "status":"running"}));
    f.watch("home");
    assert_eq!(f.app.selected_target, Target::Home);
    assert!(f.attachment.task.is_none());
    assert_ne!(attachment_target(&f.app, "test".into(), None), Target::Home);
    let (replacement, _peers) = client::DaemonClient::fake_with_identity(
        json!({"session_id":"sess-other", "home_run_id":"chat-reserved"}),
    );
    install_home_session(&mut f.app, &replacement).await;
    assert_eq!(
        attachment_target(&f.app, "chat-reserved".into(), None),
        Target::Home
    );
    assert_ne!(attachment_target(&f.app, "home".into(), None), Target::Home);
    f.assert_unsent();
    replacement.shutdown().await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn unknown_stage_attaches_run_only_and_adopts_ack_target() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    f.debug("unlisted");
    let (stream, _) = listener.accept().await.unwrap();
    let mut peer = BufReader::new(stream);
    let request = read(&mut peer).await;
    assert_eq!(request["id"], "unlisted");
    assert!(request.get("stage").is_none());
    f.receive(&mut peer, json!({"type":"attached", "scope":"conversation", "id":"unlisted", "stage":"review", "execution_id":"exec", "nonce":9, "capabilities":["message"]})).await;
    let target = Target::StageExecution {
        run_id: "unlisted".into(),
        stage: "review".into(),
    };
    assert_eq!(f.app.selected_target, target);
    assert_eq!(f.attachment.target, Some(target));
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "text":"adopted stream"}),
    )
    .await;
    assert_eq!(f.live.response_text(), "adopted stream");
    f.app.replace_input("correction");
    let Submission::Correction { id, execution, .. } =
        f.app
            .conversations
            .submit(&f.app.selected_target, &f.app.input, false)
    else {
        panic!("expected correction")
    };
    assert!(f
        .attachment
        .routes_correction(&f.app.selected_target, &execution));
    let mut successor = execution.clone();
    successor.nonce += 1;
    assert!(!f
        .attachment
        .routes_correction(&f.app.selected_target, &successor));
    f.receive(
        &mut peer,
        json!({"type":"error", "req_id":"selected-control", "message":"cannot continue"}),
    )
    .await;
    assert!(matches!(
        f.app
            .conversations
            .submit(&f.app.selected_target, &f.app.input, false),
        Submission::Blocked("delivery is pending for this conversation")
    ));
    assert!(f.app.finish_delivery(id, Ok(())).is_some());
    drop(peer);
    f.frame().await;
    assert_ne!(
        f.app.conversation_status(&f.app.selected_target),
        ConversationStatus::Ended
    );
    assert_eq!(f.live.response_text(), "adopted stream");
    assert!(f.attachment.task.is_none());
    assert!(f.attachment.execution.is_none());
    assert!(f.attachment.commands.is_none());
    let rendered = f.rendered();
    assert!(rendered.contains("attachment disconnected"), "{rendered}");
    assert!(rendered.contains("adopted stream"), "{rendered}");
    assert!(!rendered.contains("ended:"), "{rendered}");
    assert_eq!(f.live.backend().buffer().area.width, 80);
    assert_eq!(f.live.backend().buffer().area.height, 24);
    assert_eq!(f.rendered(), rendered);
    f.client.shutdown().await;
}

#[tokio::test]
async fn attachment_disconnect_preserves_current_turn() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    let mut peer = f.attach_debug(&listener).await;
    f.live.submit_prompt("question".into());
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "text":"partial response"}),
    )
    .await;
    assert!(f.live.has_current_turn());
    drop(peer);
    f.frame().await;
    assert!(f.live.has_current_turn());
    assert_eq!(f.live.response_text(), "partial response");
    assert_ne!(
        f.app.conversation_status(&f.app.selected_target),
        ConversationStatus::Ended
    );
    let rendered = f.rendered();
    assert!(rendered.contains("attachment disconnected"), "{rendered}");
    assert!(rendered.contains("partial response"), "{rendered}");
    assert!(!rendered.contains("ended:"), "{rendered}");
    assert!(f.live.has_current_turn());
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_debug_without_run_reports_why_it_cannot_attach() {
    let mut f = Fixture::new();
    select_attachment(
        &mut f.app,
        &mut f.live,
        &f.client,
        Target::Home,
        "debugger",
        &mut f.attachment,
        &f.tx,
        &mut f.last_stage,
    );
    assert!(f.rendered().contains("home has no chat run to debug yet"));
    assert!(f.attachment.task.is_none());
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_debug_done_defers_summary_and_lifecycle_to_owner() {
    let mut f = Fixture::new();
    f.app.active_request = true;
    f.live.submit_prompt("question".into());
    f.live.push_response("owner response");
    let execution = protocol::Execution {
        run_id: "chat-0001".into(),
        execution_id: "owner".into(),
        nonce: 1,
    };
    f.attachment.target = Some(Target::Home);
    f.attachment.execution = Some(execution.clone());
    apply_attachment_frame(
        &mut f.app,
        &mut f.live,
        &mut f.attachment,
        AttachmentFrame {
            generation: 0,
            target: Target::Home,
            execution: Some(execution),
            value: json!({"type":"done", "text":"debug response", "summary":"debug summary"}),
        },
    );
    assert!(f.app.active_request);
    assert_eq!(f.live.response_text(), "owner response");
    assert!(f.live.has_current_turn());
    assert!(!f.rendered().contains("debug summary"));
    f.client.shutdown().await;
}

#[tokio::test]
async fn watch_partial_correction_then_home_run_completes_without_execution_control() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    install_home_session(&mut f.app, &f.client).await;
    f.app.replace_input("home draft");
    let mut peer = f.attach_watch(&listener, "original").await;
    let target = f.app.selected_target.clone();
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::Running
    );
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "text":"partial answer"}),
    )
    .await;
    assert_eq!(f.live.response_text(), "partial answer");
    assert_eq!(f.rendered().matches("partial answer").count(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), socket::read_json_line(&mut peer))
            .await
            .is_err()
    );
    f.app.replace_input("use another approach");
    let Submission::Correction {
        id,
        text,
        execution,
    } = f.app.conversations.submit(&target, &f.app.input, false)
    else {
        panic!("expected correction")
    };
    assert_eq!(execution.execution_id, "original");
    send_attachment_correction(&mut f.app, &mut f.live, &f.attachment, id, &text);
    assert_eq!(
        read(&mut peer).await,
        json!({"op":"message", "req_id":id.to_string(), "text":"use another approach"})
    );
    f.receive(
        &mut peer,
        json!({"type":"operator_message", "text":"use another approach"}),
    )
    .await;
    f.receive(
        &mut peer,
        json!({"type":"delivered", "req_id":id.to_string()}),
    )
    .await;
    assert!(f.app.input.is_empty());
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "text":"corrected answer"}),
    )
    .await;
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::Running
    );
    let rendered = f.rendered();
    for text in ["use another approach", "corrected answer"] {
        assert_eq!(rendered.matches(text).count(), 1, "{rendered}");
    }
    assert!(f.control("/exit"));
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.input.text(), "home draft");
    // The fixture finishes after the observer leaves, without receiving a
    // cancel/continue/quit (or any other command) on the attachment socket.
    let run = tokio::spawn(async move {
        assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        json!({"type":"run_completed", "id":"run"})
    });
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::Running
    );
    let completed = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("detached fixture run did not complete autonomously")
        .unwrap();
    assert_eq!(completed["type"], "run_completed");
    f.app
        .on_run_completed(completed["id"].as_str().unwrap().into());
    assert_eq!(f.app.active_runs["run"].status, "done");
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.screen, Screen::Chat);
    assert_eq!(f.app.input.text(), "home draft");
    assert!(f.attachment.target.is_none());
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn ended_watch_draft_survives_same_nonce_successor_without_sending() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    let mut original = f.attach_watch(&listener, "original").await;
    f.app.replace_input("unsent correction");
    f.receive(
        &mut original,
        json!({"type":"done", "text":"original complete"}),
    )
    .await;
    assert_eq!(f.app.input.text(), "unsent correction");
    let mut successor = f.attach_watch(&listener, "successor").await;
    assert_eq!(f.app.input.text(), "unsent correction");
    assert!(matches!(
        f.app
            .conversations
            .submit(&f.app.selected_target, &f.app.input, false),
        Submission::Blocked(
            "the addressed execution has changed; dismiss this draft to start another"
        )
    ));
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        socket::read_json_line(&mut successor)
    )
    .await
    .is_err());
    assert_eq!(f.app.input.text(), "unsent correction");
    f.assert_unsent();
    f.attachment.invalidate(&mut f.app);
    f.client.shutdown().await;
}

#[tokio::test]
async fn rapid_watch_home_watch_rejects_buffered_old_generation_for_same_execution() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    install_home_session(&mut f.app, &f.client).await;
    let mut first = f.attach_watch(&listener, "same-execution").await;
    socket::write_json_line(
        first.get_mut(),
        &json!({"type":"stream_chunk", "text":"stale buffered output"}),
    )
    .await
    .unwrap();
    let buffered = tokio::time::timeout(Duration::from_secs(2), f.rx.recv())
        .await
        .unwrap()
        .unwrap();
    f.watch("home");
    let mut current = f.attach_watch(&listener, "same-execution").await;
    assert_ne!(buffered.generation, f.attachment.generation);
    assert_eq!(buffered.execution, f.attachment.execution);
    apply_attachment_frame(&mut f.app, &mut f.live, &mut f.attachment, buffered);
    f.receive(
        &mut current,
        json!({"type":"stream_chunk", "text":"current output"}),
    )
    .await;
    assert_eq!(f.live.response_text(), "current output");
    let rendered = f.rendered();
    assert!(!rendered.contains("stale buffered output"), "{rendered}");
    assert_eq!(rendered.matches("current output").count(), 1, "{rendered}");
    f.assert_unsent();
    f.attachment.invalidate(&mut f.app);
    f.client.shutdown().await;
}

struct Sandbox {
    _lock: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl Sandbox {
    fn new() -> Self {
        let lock = test_support::CONFIG_ENV_LOCK.lock().unwrap();
        // A short path also fits macOS's Unix socket path limit.
        let dir = tempfile::Builder::new()
            .prefix("entry-")
            .tempdir_in("/tmp")
            .unwrap();
        let previous = std::env::var_os("GREMLINS_SANDBOX_ROOT");
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        gremlins::config::clear_global();
        Self {
            _lock: lock,
            _dir: dir,
            previous,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("GREMLINS_SANDBOX_ROOT", value),
            None => std::env::remove_var("GREMLINS_SANDBOX_ROOT"),
        }
        gremlins::config::clear_global();
    }
}

async fn read(peer: &mut BufReader<UnixStream>) -> Value {
    tokio::time::timeout(Duration::from_secs(2), socket::read_json_line(peer))
        .await
        .expect("socket request timed out")
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn debug_waiting_for_ready_can_finish_without_ready() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    for streamed in [false, true] {
        let mut f = Fixture::new();
        let mut peer = f.attach_debug(&listener).await;
        let target = f.app.selected_target.clone();
        f.receive(&mut peer, json!({"type":"waiting_for_ready"}))
            .await;
        if streamed {
            f.receive(
                &mut peer,
                json!({"type":"stream_chunk", "text":"final answer"}),
            )
            .await;
            assert_eq!(f.live.response_text(), "final answer");
        }
        let done = json!({"type":"done", "text":"final answer", "summary":"distinct summary"});
        // Retain the original identity to exercise handler-level duplicate rejection,
        // even though the socket reader stops after the first terminal frame.
        let duplicate = AttachmentFrame {
            generation: f.attachment.generation,
            target: target.clone(),
            execution: f.attachment.execution.clone(),
            value: done.clone(),
        };
        f.receive(&mut peer, done).await;
        assert_eq!(
            f.app.conversation_status(&target),
            ConversationStatus::Ended
        );
        assert!(f.attachment.target.is_none());
        let rendered = f.rendered();
        for text in ["final answer", "distinct summary"] {
            assert_eq!(rendered.matches(text).count(), 1, "{rendered}");
        }
        apply_attachment_frame(&mut f.app, &mut f.live, &mut f.attachment, duplicate);
        assert_eq!(
            f.app.conversation_status(&target),
            ConversationStatus::Ended
        );
        assert_eq!(f.rendered(), rendered);
        f.assert_unsent();
        f.client.shutdown().await;
    }
}

#[tokio::test]
async fn debug_quit_resumed_keeps_attachment_and_current_response() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    let mut peer = f.attach_debug(&listener).await;
    let target = f.app.selected_target.clone();
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "text":"preserved prefix "}),
    )
    .await;
    f.receive(&mut peer, json!({"type":"ready"})).await;
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::Paused
    );
    let generation = f.attachment.generation;
    let execution = f.attachment.execution.clone();
    assert!(f.control("/quit"));
    assert_eq!(
        read(&mut peer).await,
        json!({"op":"quit", "req_id":"selected-control"})
    );
    f.receive(
        &mut peer,
        json!({"type":"ended", "scope":"conversation", "reason":"resumed"}),
    )
    .await;
    assert_eq!(f.app.selected_target, target);
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::Running
    );
    assert_eq!(f.attachment.generation, generation);
    assert_eq!(f.attachment.target.as_ref(), Some(&target));
    assert_eq!(f.attachment.execution, execution);
    assert!(f.attachment.task.is_some());
    assert!(f.attachment.commands.is_some());
    assert_eq!(f.live.response_text(), "preserved prefix ");
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "text":"final answer"}),
    )
    .await;
    assert_eq!(f.live.response_text(), "preserved prefix final answer");
    f.receive(&mut peer, json!({"type":"done", "text":"preserved prefix final answer", "summary":"distinct summary"})).await;
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::Ended
    );
    assert!(f.attachment.target.is_none());
    let rendered = f.rendered();
    for text in ["preserved prefix", "final answer", "distinct summary"] {
        assert_eq!(rendered.matches(text).count(), 1, "{rendered}");
    }
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn watch_and_debug_use_aux_modes_controls_and_plain_corrections() {
    let _sandbox = Sandbox::new();
    let listener = socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
    let mut f = Fixture::new();
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
    for mode in ["control", "debugger"] {
        if mode == "control" {
            f.watch("run");
        } else {
            f.debug("run");
        }
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut peer = BufReader::new(stream);
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"attach", "scope":"conversation", "id":"run", "stage":"review", "session_id":"test", "mode":mode})
        );
        socket::write_json_line(peer.get_mut(), &json!({"type":"attached", "scope":"conversation", "id":"run", "stage":"review", "execution_id":"exec", "nonce":9, "capabilities":["message"]})).await.unwrap();
        f.frame().await;
        assert_eq!(f.app.screen, Screen::Chat);
        assert_eq!(
            f.app.conversation_status(&f.app.selected_target),
            ConversationStatus::Running
        );
        if mode == "debugger" {
            socket::write_json_line(peer.get_mut(), &json!({"type":"ready"}))
                .await
                .unwrap();
            f.frame().await;
            assert_eq!(
                f.app.conversation_status(&f.app.selected_target),
                ConversationStatus::Paused
            );
        }
        for (input, expected) in [
            (
                "/continue",
                json!({"op":"continue", "req_id":"selected-control"}),
            ),
            ("/quit", json!({"op":"quit", "req_id":"selected-control"})),
            (
                "/quit stop now",
                json!({"op":"bail", "reason":"stop now", "req_id":"selected-control"}),
            ),
            (
                "/bail enough",
                json!({"op":"bail", "reason":"enough", "req_id":"selected-control"}),
            ),
        ] {
            assert!(f.control(input));
            assert!(f.app.input.is_empty());
            assert_eq!(read(&mut peer).await, expected);
        }
        assert!(!f.control("ordinary correction"));
        let target = f.app.selected_target.clone();
        let Submission::Correction {
            id,
            execution,
            text,
        } = f.app.conversations.submit(&target, &f.app.input, false)
        else {
            panic!("expected correction")
        };
        assert_eq!(execution.execution_id, "exec");
        assert_eq!(execution.nonce, 9);
        send_attachment_correction(&mut f.app, &mut f.live, &f.attachment, id, &text);
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"message", "req_id":id.to_string(), "text":"ordinary correction"})
        );
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"delivered", "req_id":id.to_string()}),
        )
        .await
        .unwrap();
        f.frame().await;
        assert!(f.app.input.is_empty());
        assert!(f.control("/exit"));
        assert_eq!(f.app.selected_target, Target::Home);
        assert!(f.attachment.target.is_none());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), socket::read_json_line(&mut peer))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        f.assert_unsent();
    }
    f.client.shutdown().await;
}
