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
    socket: Option<(tokio::net::UnixListener, Sandbox)>,
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
            socket: None,
        }
    }

    fn with_socket() -> Self {
        let sandbox = Sandbox::new();
        let listener =
            socket::bind_session_socket(&gremlins::config::state_root(), "test").unwrap();
        let mut f = Self::new();
        f.socket = Some((listener, sandbox));
        f
    }

    fn with_watch_socket(count: usize) -> Self {
        let mut f = Self::with_socket();
        let path = f.socket.as_ref().unwrap().0.local_addr().unwrap();
        let streams = (0..count)
            .map(|_| {
                let stream =
                    std::os::unix::net::UnixStream::connect(path.as_pathname().unwrap()).unwrap();
                stream.set_nonblocking(true).unwrap();
                UnixStream::from_std(stream).unwrap()
            })
            .collect();
        let (client, peers) = client::DaemonClient::fake_with_aux_streams(
            json!({"session_id":"test", "home_run_id":"home"}),
            streams,
        );
        f.client = Arc::new(client);
        f.peers = peers;
        f
    }

    async fn peer(&self) -> BufReader<UnixStream> {
        let listener = &self.socket.as_ref().unwrap().0;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        BufReader::new(stream)
    }

    async fn assert_closed(&self, peer: &mut BufReader<UnixStream>) {
        assert!(
            tokio::time::timeout(Duration::from_secs(2), socket::read_json_line(peer))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(self.attachment.target.is_none());
        assert!(self.attachment.task.is_none());
        assert!(self.attachment.commands.is_none());
        self.assert_unsent();
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

    fn home(&mut self) {
        close_watch_follow(&mut self.app, &mut self.live, &mut self.follow);
        select_passive_watch(
            &mut self.app,
            &mut self.live,
            &self.client,
            Target::Home,
            &mut self.attachment,
            &self.tx,
            &mut self.last_stage,
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

    async fn attach(&mut self, mode: &str, stage_run_id: &str) -> BufReader<UnixStream> {
        self.app
            .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
        match mode {
            "debugger" => self.debug("run"),
            _ => panic!("unexpected attachment mode: {mode}"),
        }
        let mut peer = self.peer().await;
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"attach", "scope":"conversation", "id":"run", "stage":"review", "session_id":"test", "mode":mode})
        );
        self.receive(&mut peer, json!({"type":"attached", "scope":"conversation", "id":"run", "stage":"review", "stage_run_id":stage_run_id, "nonce":9, "capabilities":["message"]})).await;
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

    async fn watch_socket(
        &mut self,
        key: &str,
        stage: &str,
        execution: Option<&str>,
    ) -> BufReader<UnixStream> {
        self.watch(key);
        let mut peer = self.peer().await;
        let mut expected =
            json!({"op":"watch", "scope":"run", "id":"run", "session_id":"test", "replay":true});
        if let Some(execution) = execution {
            expected["stage"] = json!(stage);
            expected["stage_run_id"] = json!(execution);
        }
        assert_eq!(read(&mut peer).await, expected);
        let selected_stage = execution.map(|_| stage);
        self.receive(&mut peer, json!({"type":"watched", "scope":"run", "id":"run", "replay":true, "stage":selected_stage, "stage_run_id":execution})).await;
        assert_eq!(self.app.screen, Screen::Chat);
        assert!(self.attachment.commands.is_none());
        assert!(self.attachment.execution.is_none());
        peer
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
async fn exact_inline_watch_renders_identity_free_lag_warning_but_rejects_sibling() {
    let mut f = Fixture::new();
    f.app.show_runs();
    f.live.show_runs();
    f.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"selected", "stage_type":"exec", "active":true}));
    let execution = f.app.live_executions.values().next().unwrap().clone();
    f.app.watch_following = Some("run".into());
    let view = f.live.runs_view_mut().unwrap();
    view.watch_execution = Some(execution);
    view.watch_pending = true;
    for (native, line) in [
        (
            json!({"type":"watched", "id":"run", "replay":true, "stage":"review", "stage_run_id":"selected"}),
            "",
        ),
        (
            json!({"type":"lagged", "skipped":3}),
            "warning: watch lagged; skipped 3 frames\n",
        ),
        (
            json!({"type":"exec_output", "run_id":"run", "stage":"review", "stage_run_id":"sibling", "text":"foreign-sibling"}),
            "foreign-sibling\n",
        ),
    ] {
        apply_watch_frame(
            &mut f.app,
            &mut f.live,
            &json!({"type":"view_log", "id":"run", "generation":7, "native":native, "line":line}),
            7,
            &mut f.follow,
        );
    }
    let rendered = f.rendered();
    assert!(
        rendered.contains("warning: watch lagged; skipped 3 frames"),
        "{rendered}"
    );
    assert!(!rendered.contains("foreign-sibling"), "{rendered}");
    close_watch_follow(&mut f.app, &mut f.live, &mut f.follow);
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_selection_preserves_idle_and_streaming_drafts_without_attachment_or_send() {
    let mut f = Fixture::new();
    install_home_session(&mut f.app, &f.client).await;
    f.app
        .upsert_run_from_ls(&json!({"id":"home", "status":"running"}));
    f.app.replace_input("unsent home draft");
    f.watch("home");
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.screen, Screen::Chat);
    assert_eq!(f.app.input.text(), "unsent home draft");
    assert!(!f.app.active_request);
    assert!(f.attachment.target.is_none());
    assert!(f.attachment.task.is_none());
    assert!(f.attachment.commands.is_none());
    assert!(!f.control("/continue"));
    f.assert_unsent();
    let owner = protocol::Execution {
        run_id: "home".into(),
        stage_run_id: "owner".into(),
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
        assert_eq!(f.rendered().matches("owner response").count(), 1);
        assert!(f.attachment.target.is_none());
        assert!(f.attachment.task.is_none());
    }
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
    f.home();
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
async fn unknown_stage_adopts_and_stores_advertised_stage_run_id() {
    let mut f = Fixture::with_socket();
    f.debug("unlisted");
    let mut peer = f.peer().await;
    let request = read(&mut peer).await;
    assert_eq!(request["id"], "unlisted");
    assert!(request.get("stage").is_none());
    f.receive(&mut peer, json!({"type":"attached", "scope":"conversation", "id":"unlisted", "stage":"review", "stage_run_id":"exec", "nonce":9, "capabilities":["message"]})).await;
    let target = Target::StageExecution {
        run_id: "unlisted".into(),
        stage: "review".into(),
    };
    assert_eq!(f.app.selected_target, target);
    assert_eq!(f.attachment.target, Some(target.clone()));
    let mut input = input_buffer::InputBuffer::new();
    input.replace_all("correction");
    let Submission::Correction { execution, .. } =
        f.app.conversations.submit(&target, &input, false)
    else {
        panic!("advertised stage_run_id must enable correction selection")
    };
    assert_eq!(execution.stage_run_id, "exec");
    assert_eq!(execution.nonce, 9);
    assert_eq!(f.attachment.execution.as_ref(), Some(&execution));
    for foreign in [
        json!({"type":"stream_chunk", "stage_run_id":"other", "nonce":9, "text":"wrong stage run"}),
        json!({"type":"stream_chunk", "stage_run_id":"exec", "nonce":10, "text":"wrong nonce"}),
    ] {
        socket::write_json_line(peer.get_mut(), &foreign)
            .await
            .unwrap();
    }
    f.receive(
        &mut peer,
        json!({"type":"stream_chunk", "stage_run_id":"exec", "nonce":9, "text":"adopted stream"}),
    )
    .await;
    assert_eq!(f.live.response_text(), "adopted stream");
    assert_eq!(
        f.attachment.execution.as_ref().unwrap().stage_run_id,
        "exec"
    );
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
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_debug_without_run_reports_why_it_cannot_attach() {
    let mut f = Fixture::new();
    select_debugger_attachment(
        &mut f.app,
        &mut f.live,
        &f.client,
        Target::Home,
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
async fn debug_partial_correction_then_exit_closes_without_execution_control() {
    let mut f = Fixture::with_socket();
    install_home_session(&mut f.app, &f.client).await;
    f.app.replace_input("home draft");
    let mut peer = f.attach("debugger", "original").await;
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
    assert!(!f.control("use another approach"));
    assert_eq!(f.live.response_text(), "partial answer");
    let Submission::Correction {
        id,
        text,
        execution,
    } = f.app.conversations.submit(&target, &f.app.input, false)
    else {
        panic!("expected correction")
    };
    assert_eq!(execution.stage_run_id, "original");
    send_attachment_correction(&mut f.app, &mut f.live, &f.attachment, id, &text);
    assert_eq!(
        f.live.response_text(),
        "partial answer",
        "sending alone must not split the transcript"
    );
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
    assert_eq!(f.live.response_text(), "corrected answer");
    for text in ["partial answer", "use another approach", "corrected answer"] {
        assert_eq!(rendered.matches(text).count(), 1, "{rendered}");
    }
    assert!(f.control("/exit"));
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.input.text(), "home draft");
    // EOF, rather than a cancel/continue/quit, is the only detach effect.
    f.assert_closed(&mut peer).await;
    assert_eq!(f.app.active_runs["run"].status, "running");
    f.client.shutdown().await;
}

#[tokio::test]
async fn rapid_debug_home_debug_rejects_buffered_old_generation_for_same_execution() {
    let mut f = Fixture::with_socket();
    install_home_session(&mut f.app, &f.client).await;
    let mut first = f.attach("debugger", "same-execution").await;
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
    f.home();
    let mut current = f.attach("debugger", "same-execution").await;
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

#[tokio::test]
async fn watch_enter_exact_invocation_outside_retention_is_passive_and_isolates_drafts() {
    let mut f = Fixture::with_watch_socket(2);
    install_home_session(&mut f.app, &f.client).await;
    f.app.replace_input("home private draft");
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"newer", "status":"running"}));
    // The row was discovered earlier; its start has since fallen outside retention.
    f.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"old", "stage_type":"exec", "active":true}));
    let key = f
        .app
        .live_executions
        .iter()
        .find(|(_, e)| e.stage_run_id == "old")
        .unwrap()
        .0
        .clone();
    let target = Target::Invocation {
        run_id: "run".into(),
        stage: "review".into(),
        stage_run_id: "old".into(),
    };
    let mut peer = f.watch_socket(&key, "review", Some("old")).await;
    assert_eq!(f.app.selected_target, target);
    assert!(f.app.input.is_empty());
    f.receive(
        &mut peer,
        json!({"type":"history_gap", "message":"start outside retention"}),
    )
    .await;
    // No start frame or nonce is needed for any identified output channel.
    for (kind, text, stderr) in [
        ("exec_output", "old-stdout\n", false),
        ("exec_output", "old-stderr\n", true),
        ("stream_chunk", "old-response\n", false),
        ("reasoning_chunk", "old-reasoning\n", false),
        ("tool_output_chunk", "old-tool\n", false),
    ] {
        for (run, stage, execution, body) in [
            ("run", "review", "sibling", "foreign-sibling\n"),
            ("run", "newer", "old", "foreign-stage\n"),
            ("other", "review", "old", "foreign-run\n"),
            ("run", "review", "old", text),
        ] {
            f.receive(&mut peer, json!({"type":kind, "run_id":run, "correlation_stage":stage, "stage_run_id":execution, "id":"tool-id", "call":"Bash", "text":body, "is_stderr":stderr})).await;
        }
    }
    f.receive(
        &mut peer,
        json!({"type":"replay_checkpoint", "id":"run", "complete":false}),
    )
    .await;
    let rendered = f.rendered();
    for text in [
        "old-stdout",
        "old-stderr",
        "old-response",
        "old-reasoning",
        "old-tool",
    ] {
        assert_eq!(rendered.matches(text).count(), 1, "{rendered}");
    }
    assert!(!rendered.contains("foreign-"), "{rendered}");
    assert!(rendered.contains("start outside retention"), "{rendered}");
    assert_eq!(f.app.active_runs["run"].stage, "newer");
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::ReadOnly
    );
    let mut input = input_buffer::InputBuffer::new();
    input.replace_all("must not become correction");
    assert!(!matches!(
        f.app.conversations.submit(&target, &input, false),
        Submission::Correction { .. }
    ));
    for command in ["/continue", "/quit", "/bail stop"] {
        assert!(f.control(command));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), socket::read_json_line(&mut peer))
            .await
            .is_err()
    );
    f.app.replace_input("invocation private draft");
    // Capture an old-generation frame, then leave and return to the same invocation.
    socket::write_json_line(peer.get_mut(), &json!({"type":"stream_chunk", "run_id":"run", "stage":"review", "stage_run_id":"old", "text":"stale-watch-output"})).await.unwrap();
    let stale = tokio::time::timeout(Duration::from_secs(2), f.rx.recv())
        .await
        .unwrap()
        .unwrap();
    f.home();
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.input.text(), "home private draft");
    f.assert_closed(&mut peer).await;
    let mut current = f.watch_socket(&key, "review", Some("old")).await;
    assert_eq!(f.app.input.text(), "invocation private draft");
    apply_attachment_frame(&mut f.app, &mut f.live, &mut f.attachment, stale);
    assert!(!f.rendered().contains("stale-watch-output"));
    assert!(f.control("/exit"));
    assert_eq!(f.app.input.text(), "home private draft");
    f.assert_closed(&mut current).await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn passive_replay_survives_closing_absent_inline_follow_until_incomplete_checkpoint() {
    let mut f = Fixture::with_watch_socket(1);
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
    let mut peer = f.watch_socket("run", "review", None).await;
    assert!(f.app.watch_following.is_none());
    assert!(f.follow.is_none());
    // Ctrl+W from Chat closes only the inline pane before showing the table.
    close_watch_follow(&mut f.app, &mut f.live, &mut f.follow);
    f.app.show_runs();
    f.live.show_runs();
    f.receive(&mut peer, json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"stale", "stage_type":"exec", "active":true})).await;
    assert!(f.app.live_executions.is_empty());
    assert_eq!(
        f.app.watch_target(),
        Some(Target::StageExecution {
            run_id: "run".into(),
            stage: String::new()
        })
    );
    f.receive(
        &mut peer,
        json!({"type":"replay_checkpoint", "id":"run", "complete":false}),
    )
    .await;
    assert!(f.app.live_executions.is_empty());
    assert_eq!(
        f.app.watch_target(),
        Some(Target::StageExecution {
            run_id: "run".into(),
            stage: String::new()
        })
    );
    assert!(f.attachment.task.is_some());
    assert!(f.attachment.commands.is_none());
    f.attachment.invalidate(&mut f.app);
    f.assert_closed(&mut peer).await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn opening_inline_follow_preserves_passive_replay_until_incomplete_checkpoint() {
    let mut f = Fixture::with_watch_socket(2);
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
    let mut peer = f.watch_socket("run", "review", None).await;
    close_watch_follow(&mut f.app, &mut f.live, &mut f.follow);
    f.app.show_runs();
    f.live.show_runs();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut generation = 0;
    start_watch_follow(
        &mut f.app,
        &mut f.live,
        &f.client,
        "run".into(),
        &tx,
        &mut f.follow,
        &mut generation,
    );
    let mut inline = f.peer().await;
    assert_eq!(read(&mut inline).await["op"], "watch");
    // The passive subscription's retained start arrives before the inline ack.
    f.receive(&mut peer, json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"stale", "stage_type":"exec", "active":true})).await;
    assert!(f.app.live_executions.is_empty());

    // Completing a different subscription must not finish the passive replay.
    socket::write_json_line(inline.get_mut(), &json!({"type":"watched", "scope":"run", "id":"run", "replay":true, "stage":null, "stage_run_id":null})).await.unwrap();
    socket::write_json_line(
        inline.get_mut(),
        &json!({"type":"replay_checkpoint", "id":"run", "complete":true}),
    )
    .await
    .unwrap();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let checkpoint = frame["native"]["type"] == "replay_checkpoint";
        apply_watch_frame(&mut f.app, &mut f.live, &frame, generation, &mut f.follow);
        if checkpoint {
            break;
        }
    }
    f.receive(&mut peer, json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"also-stale", "stage_type":"exec", "active":true})).await;
    assert!(f.app.live_executions.is_empty());
    f.receive(
        &mut peer,
        json!({"type":"replay_checkpoint", "id":"run", "complete":false}),
    )
    .await;
    assert!(f.app.live_executions.is_empty());
    assert_eq!(f.app.watch_ids(), vec!["run"]);
    assert_eq!(
        f.app.watch_target(),
        Some(Target::StageExecution {
            run_id: "run".into(),
            stage: String::new(),
        })
    );
    assert!(f.attachment.task.is_some());
    assert!(f.attachment.commands.is_none());
    close_watch_follow(&mut f.app, &mut f.live, &mut f.follow);
    f.attachment.invalidate(&mut f.app);
    f.assert_closed(&mut peer).await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn attach_authoritative_home_preserves_idle_and_streaming_drafts_without_socket() {
    let mut f = Fixture::with_socket();
    install_home_session(&mut f.app, &f.client).await;
    assert_eq!(f.app.home_run_id.as_deref(), Some("home"));
    for streaming in [false, true] {
        f.app.active_request = streaming;
        if streaming {
            f.live.submit_prompt("question".into());
            f.live.push_response("home streaming response");
        }
        let draft = if streaming {
            "streaming draft"
        } else {
            "idle draft"
        };
        f.app.replace_input(draft);
        for stage in [None, Some("chat".into())] {
            attach_stage_entry(
                &mut f.app,
                &mut f.live,
                &f.client,
                "home".into(),
                stage,
                &mut f.attachment,
                &f.tx,
                &mut f.last_stage,
            );
            assert_eq!(f.app.selected_target, Target::Home);
            assert_eq!(f.app.screen, Screen::Chat);
            assert_eq!(f.app.input.text(), draft);
            assert_eq!(f.app.active_request, streaming);
            if streaming {
                assert_eq!(f.live.response_text(), "home streaming response");
            }
            assert!(f.attachment.target.is_none());
            assert!(f.attachment.task.is_none());
            assert!(f.attachment.commands.is_none());
            f.assert_unsent();
        }
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            f.socket.as_ref().unwrap().0.accept()
        )
        .await
        .is_err(),
        "home attach must not open an auxiliary socket"
    );
    f.client.shutdown().await;
}

#[tokio::test]
async fn run_only_attach_command_does_not_infer_current_stage() {
    let mut f = Fixture::with_watch_socket(1);
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
    let Some(commands::CommandResult::AttachStage { run_id, stage }) =
        commands::dispatch("/attach run")
    else {
        panic!("expected attach command");
    };
    attach_stage_entry(
        &mut f.app,
        &mut f.live,
        &f.client,
        run_id,
        stage,
        &mut f.attachment,
        &f.tx,
        &mut f.last_stage,
    );
    assert_eq!(
        f.app.selected_target,
        Target::StageExecution {
            run_id: "run".into(),
            stage: String::new(),
        }
    );
    let mut peer = f.peer().await;
    assert_eq!(
        read(&mut peer).await,
        json!({"op":"watch", "scope":"run", "id":"run", "session_id":"test", "replay":true})
    );
    f.receive(&mut peer, json!({"type":"watched", "scope":"run", "id":"run", "replay":true, "stage":null, "stage_run_id":null})).await;
    assert!(f.attachment.commands.is_none());
    f.attachment.invalidate(&mut f.app);
    f.assert_closed(&mut peer).await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn watch_run_row_routes_interleaved_sibling_invocations_once() {
    let mut f = Fixture::with_watch_socket(1);
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
    let mut peer = f.watch_socket("run", "review", None).await;
    for (kind, label) in [
        ("exec_output", "stdout"),
        ("exec_output", "stderr"),
        ("stream_chunk", "response"),
        ("reasoning_chunk", "reasoning"),
        ("tool_output_chunk", "tool"),
    ] {
        for execution in ["a", "b"] {
            f.receive(&mut peer, json!({"type":kind, "run_id":"run", "stage":"review", "stage_run_id":execution, "id":format!("tool-{execution}"), "call":"Bash", "text":format!("{execution}-{label}\n"), "is_stderr":label == "stderr"})).await;
        }
    }
    let rendered = f.rendered();
    for execution in ["a", "b"] {
        for label in ["stdout", "stderr", "response", "reasoning", "tool"] {
            assert_eq!(
                rendered.matches(&format!("{execution}-{label}")).count(),
                1,
                "{rendered}"
            );
        }
        let target = Target::Invocation {
            run_id: "run".into(),
            stage: "review".into(),
            stage_run_id: execution.into(),
        };
        // Render each parked target on a fresh terminal: previous screen cells and
        // terminal scrollback are not part of that invocation's transcript.
        let mut isolated_live = LiveArea::with_backend(TestBackend::new(80, 24), 80, 24);
        f.live.with_target_transcript(&target, |live, reasoning| {
            isolated_live.with_target_transcript(&Target::Home, |transcript, band| {
                std::mem::swap(transcript, live);
                std::mem::swap(band, reasoning);
            });
        });
        isolated_live.compose(&f.app).unwrap();
        let isolated = isolated_live
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in ["stdout", "stderr", "response", "reasoning", "tool"] {
            assert_eq!(
                isolated.matches(&format!("{execution}-{label}")).count(),
                1,
                "{isolated}"
            );
        }
        let sibling = if execution == "a" { "b" } else { "a" };
        assert!(!isolated.contains(&format!("{sibling}-")), "{isolated}");
        assert_eq!(
            f.app.conversation_status(&target),
            ConversationStatus::ReadOnly
        );
    }
    f.attachment.invalidate(&mut f.app);
    f.assert_closed(&mut peer).await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn watch_rejection_closes_auxiliary_socket_without_attach_or_control_fallback() {
    let mut f = Fixture::with_watch_socket(1);
    f.app
        .upsert_run_from_ls(&json!({"id":"run", "stage":"review", "status":"running"}));
    f.watch("run");
    let mut peer = f.peer().await;
    assert_eq!(
        read(&mut peer).await,
        json!({"op":"watch", "scope":"run", "id":"run", "session_id":"test", "replay":true})
    );
    f.receive(
        &mut peer,
        json!({"type":"error", "message":"watch rejected"}),
    )
    .await;
    assert!(f.rendered().contains("watch rejected"));
    assert!(f.attachment.commands.is_none());
    assert!(f.attachment.execution.is_none());
    for command in ["/continue", "/quit", "/bail stop"] {
        assert!(f.control(command));
    }
    assert!(f.control("/exit"));
    f.assert_closed(&mut peer).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            f.socket.as_ref().unwrap().0.accept()
        )
        .await
        .is_err(),
        "watch rejection must not open a fallback attachment"
    );
    f.assert_unsent();
    f.client.shutdown().await;
}

// Exercise the same routing and Home-event entry points as the event loop.
impl Fixture {
    fn start_home(&mut self, turn: &str) {
        self.app.clear_input();
        self.app.conversations.begin_home_turn(turn.into());
        self.app.active_request = true;
        self.live.submit_prompt(format!("prompt {turn}"));
    }

    fn home_event(
        &mut self,
        active: &mut Option<String>,
        turn: &str,
        value: Value,
        execution: Option<protocol::Execution>,
    ) {
        apply_home_event(
            &mut self.app,
            &mut self.live,
            active,
            &mut HomeInterrupt::default(),
            serde_json::from_value(value).unwrap(),
            Some(turn.into()),
            execution,
        )
        .unwrap();
    }

    fn submit(
        &mut self,
        tx: &mpsc::UnboundedSender<(u64, Result<(), String>)>,
        busy: bool,
    ) -> bool {
        route_submission(
            &mut self.app,
            &mut self.live,
            &self.client,
            &self.attachment,
            tx,
            busy,
        )
    }

    // Native bus frames have correlation_turn (not EventKind's numeric turn).
    fn home_bus(&mut self, active: &mut Option<String>, frame: Value) {
        let turn = frame["correlation_turn"].as_str().unwrap().to_owned();
        let execution = (frame["stage_type"] == "agent")
            .then(|| protocol::Execution::from_bus_frame(&frame))
            .flatten();
        self.home_event(active, &turn, frame, execution);
    }

    // Use the public connection path so tests consume the real output queues,
    // rather than recreating transport warnings or authoritative identities.
    async fn connect_home_queues(
        &mut self,
    ) -> (
        mpsc::UnboundedReceiver<client::ChatEvent>,
        mpsc::UnboundedReceiver<Value>,
    ) {
        let listener = socket::bind_socket(&gremlins::config::state_root()).unwrap();
        let session_listener = &self.socket.as_ref().unwrap().0;
        let server = async {
            let (control, _) = listener.accept().await.unwrap();
            let mut control = BufReader::new(control);
            assert_eq!(read(&mut control).await, json!({"op":"version"}));
            socket::write_json_line(control.get_mut(), &json!({"version":gremlins::VERSION}))
                .await
                .unwrap();
            assert_eq!(read(&mut control).await, json!({"op":"create_session"}));
            socket::write_json_line(
                control.get_mut(),
                &json!({"type":"ok", "session_id":"test", "home_run_id":"home"}),
            )
            .await
            .unwrap();
            let (session, _) = session_listener.accept().await.unwrap();
            [control.into_inner(), session]
        };
        let (connected, peers) = tokio::join!(client::connect(), server);
        let (client, _events, chat, raw) = connected.unwrap();
        self.client.shutdown().await;
        self.client = Arc::new(client);
        self.peers = peers;
        (chat, raw)
    }

    async fn native_input_peer(
        &self,
        run: &str,
        execution: &str,
    ) -> (BufReader<UnixStream>, Value) {
        let mut peer = self.peer().await;
        let request = read(&mut peer).await;
        assert_eq!(
            request,
            json!({"op":"input", "session_id":"test", "id":run,
            "stage_run_id":execution, "req_id":request["req_id"]})
        );
        assert!(request["req_id"].as_str().unwrap().parse::<u64>().is_ok());
        (peer, request)
    }

    async fn assert_no_fallback(&self) {
        self.assert_unsent();
        assert!(tokio::time::timeout(
            Duration::from_millis(30),
            self.socket.as_ref().unwrap().0.accept()
        )
        .await
        .is_err());
        assert!(self.attachment.target.is_none());
    }

    async fn correction_peer(&self) -> BufReader<UnixStream> {
        let mut peer = self.peer().await;
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"attach", "scope":"conversation", "mode":"control", "id":"home", "stage_run_id":"exec-A", "nonce":23, "session_id":"test"})
        );
        peer
    }
}

fn home_execution() -> protocol::Execution {
    protocol::Execution {
        run_id: "home".into(),
        stage_run_id: "exec-A".into(),
        nonce: 23,
    }
}

async fn delivery(
    rx: &mut mpsc::UnboundedReceiver<(u64, Result<(), String>)>,
) -> (u64, Result<(), String>) {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn home_idle_prompt_routes_to_chat_without_sending_from_router() {
    let mut f = Fixture::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.app.replace_input("new home prompt");
    assert!(!f.submit(&tx, false));
    assert_eq!(f.app.input.text(), "new home prompt");
    assert!(rx.try_recv().is_err());
    f.assert_unsent();
}

#[tokio::test]
async fn home_idle_submission_sends_exactly_one_real_chat_request() {
    let mut f = Fixture::with_watch_socket(1);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (model_tx, _model_rx) = mpsc::unbounded_channel();
    let mut generation = 0;
    let mut active = None;
    f.app.replace_input("hello home");
    assert!(!f.submit(&tx, false));
    f.assert_unsent();
    let input = f.app.input.text().to_owned();
    f.app.clear_input();
    send_chat_input(
        &mut f.live,
        &mut f.app,
        &f.client,
        input,
        &mut generation,
        &mut active,
        0,
        model_tx,
    )
    .unwrap();
    assert_eq!(active.as_deref(), Some("1"));
    assert!(f.app.active_request);

    let mut observer = f.peer().await;
    assert_eq!(
        read(&mut observer).await,
        json!({"op":"watch","scope":"run","id":"home","session_id":"test","replay":true})
    );
    for frame in [
        json!({"type":"watched","scope":"run","id":"home","stage":null,"stage_run_id":null,"replay":true}),
        json!({"type":"replay_checkpoint"}),
    ] {
        socket::write_json_line(observer.get_mut(), &frame)
            .await
            .unwrap();
    }
    let mut session = BufReader::new(&mut f.peers[1]);
    let request = tokio::time::timeout(
        Duration::from_secs(10),
        socket::read_json_line(&mut session),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(request["op"], "chat");
    assert_eq!(request["text"], "hello home");
    assert_eq!(request["turn"], "1");
    assert_eq!(request["session_id"], "test");
    socket::write_json_line(
        session.get_mut(),
        &json!({"type":"error","message":"setup failed"}),
    )
    .await
    .unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(50),
        socket::read_json_line(&mut session)
    )
    .await
    .is_err());
    f.app.replace_input("queued draft");
    assert!(f.submit(&tx, true));
    assert_eq!(f.app.input.text(), "queued draft");
    assert!(rx.try_recv().is_err());
    f.assert_unsent();
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        f.socket.as_ref().unwrap().0.accept()
    )
    .await
    .is_err());
    f.home_event(
        &mut active,
        "1",
        json!({"type":"error","message":"setup failed"}),
        None,
    );
    assert!(active.is_none());
    assert!(!f.app.active_request);
    assert_eq!(f.app.selected_target, Target::Home);
    assert_eq!(f.app.input.text(), "queued draft");
    assert!(f.submit(&tx, false)); // Never repurpose the failed turn's correction.
    f.app.clear_input();
    f.app.replace_input("fresh prompt");
    assert!(!f.submit(&tx, false));
    assert!(f.rendered().contains("setup failed"));
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_stale_task_progress_cannot_render_or_rebind_replacement() {
    let mut f = Fixture::new();
    f.start_home("B");
    let mut active = Some("B".into());
    f.app.replace_input("B draft");
    let before = f.rendered();
    f.home_event(
        &mut active,
        "A",
        json!({
            "type":"task_progress", "parent_id":"stale-task", "chain":"a.b",
            "kind":"Scout", "call":"STALE task", "reasoning":"STALE reasoning", "text":"STALE text"
        }),
        Some(home_execution()),
    );
    assert_eq!(f.rendered(), before);
    assert_eq!(active.as_deref(), Some("B"));
    assert!(f.app.active_request);
    assert_eq!(f.app.input.text(), "B draft");
    assert!(matches!(
        f.app
            .conversations
            .submit(&Target::Home, &f.app.input, true),
        Submission::Blocked { .. }
    ));
    f.assert_unsent();
}

#[tokio::test]
async fn home_queued_draft_waits_for_any_agent_execution_then_delivers_exact_correction() {
    let mut f = Fixture::with_socket();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut active = Some("A".into());
    f.start_home("A");
    f.app.replace_input("  stop here\n");
    assert!(f.submit(&tx, true));
    f.assert_unsent();
    assert!(rx.try_recv().is_err());
    assert_eq!(f.app.input.text(), "  stop here\n");
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        f.socket.as_ref().unwrap().0.accept()
    )
    .await
    .is_err());

    // No TurnStarted or capabilities: an ordinary agent frame carries identity.
    f.home_event(
        &mut active,
        "A",
        json!({"type":"stream_chunk","text":"working"}),
        Some(home_execution()),
    );
    assert!(f.submit(&tx, true));
    let mut peer = f.correction_peer().await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"attached","scope":"conversation","id":"home","stage_run_id":"exec-A","nonce":23,"capabilities":["observe","message"]})).await.unwrap();
    let command = read(&mut peer).await;
    assert_eq!(
        command,
        json!({"op":"message","req_id":command["req_id"],"text":"  stop here\n"})
    );
    assert!(command["req_id"].as_str().unwrap().parse::<u64>().is_ok());
    socket::write_json_line(peer.get_mut(), &json!({"type":"delivered","scope":"conversation","id":"home","stage_run_id":"exec-A","nonce":23,"req_id":command["req_id"]})).await.unwrap();
    let (id, result) = delivery(&mut rx).await;
    assert_eq!(id.to_string(), command["req_id"].as_str().unwrap());
    assert!(result.is_ok());
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert!(f.app.input.is_empty());
    assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
    f.assert_unsent();
}

#[tokio::test]
async fn home_rejected_corrections_preserve_draft_and_require_explicit_retry() {
    let mut f = Fixture::with_socket();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut active = Some("A".into());
    f.start_home("A");
    f.home_event(
        &mut active,
        "A",
        json!({"type":"reasoning_chunk","text":"thinking"}),
        Some(home_execution()),
    );
    f.app.replace_input("keep this correction");
    for reason in ["busy", "unsupported", "not_ready"] {
        assert!(f.submit(&tx, true));
        let mut peer = f.correction_peer().await;
        socket::write_json_line(peer.get_mut(), &json!({"type":"error","message":reason}))
            .await
            .unwrap();
        let (id, result) = delivery(&mut rx).await;
        assert!(result.as_ref().unwrap_err().contains(reason));
        f.live
            .show_receipt(f.app.finish_delivery(id, result).unwrap());
        assert_eq!(f.app.input.text(), "keep this correction");
        assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        assert!(rx.try_recv().is_err());
        assert!(tokio::time::timeout(
            Duration::from_millis(30),
            f.socket.as_ref().unwrap().0.accept()
        )
        .await
        .is_err());
        f.assert_unsent();
    }
}

#[tokio::test]
async fn home_late_terminals_cannot_finish_or_rebind_active_replacement() {
    let mut f = Fixture::new();
    f.start_home("B");
    let mut active = Some("B".into());
    let b = protocol::Execution {
        stage_run_id: "exec-B".into(),
        nonce: 24,
        ..home_execution()
    };
    f.home_event(
        &mut active,
        "B",
        json!({"type":"stream_chunk","text":"B response"}),
        Some(b.clone()),
    );
    f.app.replace_input("B draft");
    for stale in [
        json!({"type":"done","text":"STALE","summary":"STALE summary"}),
        json!({"type":"ended","reason":"STALE"}),
        json!({"type":"error","message":"STALE"}),
    ] {
        f.home_event(&mut active, "A", stale, Some(home_execution()));
        assert_eq!(active.as_deref(), Some("B"));
        assert!(f.app.active_request);
        assert_eq!(f.app.input.text(), "B draft");
        assert!(f.live.target_has_current_turn(&Target::Home));
        assert!(!f.rendered().contains("STALE"));
    }
    assert!(
        matches!(f.app.conversations.submit(&Target::Home, &f.app.input, true), Submission::Correction { execution, .. } if execution == b)
    );
    f.assert_unsent();
}

#[tokio::test]
async fn home_events_require_nonempty_matching_turn_before_any_mutation() {
    for active_id in [Some("B"), Some(""), None] {
        for event_turn in [None, Some(""), Some("A")] {
            let mut f = Fixture::new();
            f.start_home("B");
            f.app.replace_input("queued correction");
            let mut active = active_id.map(str::to_owned);
            let mut interrupt = HomeInterrupt::default();
            for event in [
                json!({"type":"turn_started","stage_run_id":"stale"}),
                json!({"type":"stream_chunk","text":"UNBOUND"}),
                json!({"type":"reasoning_chunk","text":"UNBOUND"}),
                json!({"type":"tool_output_chunk","id":"tool","name":"Read","call":"Read(x)","text":"UNBOUND"}),
                json!({"type":"tool_result","id":"tool","name":"Read","call":"Read(x)","output":"UNBOUND"}),
                json!({"type":"task_progress","parent_id":"task","chain":"x","kind":"Scout","call":"Task(x)","reasoning":"UNBOUND","text":"UNBOUND"}),
                json!({"type":"turn_complete","turn":1,"text":"UNBOUND","tool_calls":[]}),
                json!({"type":"system_warning","message":"UNBOUND"}),
                json!({"type":"done","text":"UNBOUND","summary":"UNBOUND"}),
                json!({"type":"ended","reason":"UNBOUND"}),
                json!({"type":"error","message":"UNBOUND"}),
            ] {
                apply_home_event(
                    &mut f.app,
                    &mut f.live,
                    &mut active,
                    &mut interrupt,
                    serde_json::from_value(event).unwrap(),
                    event_turn.map(str::to_owned),
                    Some(home_execution()),
                )
                .unwrap();
                assert_eq!(active.as_deref(), active_id);
                assert!(f.app.active_request);
                assert!(f.live.target_has_current_turn(&Target::Home));
                assert_eq!(f.app.input.text(), "queued correction");
                assert!(!f.rendered().contains("UNBOUND"));
                assert!(matches!(
                    f.app
                        .conversations
                        .submit(&Target::Home, &f.app.input, true),
                    Submission::Blocked(_)
                ));
            }
            f.assert_unsent();
        }
    }
}

#[tokio::test]
async fn home_tool_result_recovers_missing_chunks_once() {
    for chunks in ["", "first\n", "first\nmissing\nlast\n"] {
        let mut f = Fixture::new();
        f.start_home("A");
        let mut active = Some("A".into());
        if !chunks.is_empty() {
            f.home_event(&mut active, "A", json!({"type":"tool_output_chunk","id":"read","name":"Read","call":"Read(x)","text":chunks}), None);
        }
        for _ in 0..2 {
            f.home_event(&mut active, "A", json!({"type":"tool_result","id":"read","name":"Read","call":"Read(x)","output":"first\nmissing\nlast\n"}), None);
        }
        let rendered = f.rendered();
        for line in ["first", "missing", "last"] {
            assert_eq!(rendered.matches(line).count(), 1, "{rendered}");
        }
        f.assert_unsent();
    }
}

#[tokio::test]
async fn home_late_delivery_for_a_cannot_clear_b_draft() {
    let mut f = Fixture::with_socket();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut active = Some("A".into());
    f.start_home("A");
    f.home_event(
        &mut active,
        "A",
        json!({"type":"stream_chunk","text":"A"}),
        Some(home_execution()),
    );
    f.app.replace_input("A correction");
    assert!(f.submit(&tx, true));
    let mut peer = f.correction_peer().await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"attached","scope":"conversation","id":"home","stage_run_id":"exec-A","nonce":23,"capabilities":["message"]})).await.unwrap();
    let command = read(&mut peer).await;
    f.home_event(
        &mut active,
        "A",
        json!({"type":"done","text":"A final","summary":""}),
        None,
    );
    f.start_home("B");
    active = Some("B".into());
    f.home_event(
        &mut active,
        "B",
        json!({"type":"stream_chunk","text":"B"}),
        Some(protocol::Execution {
            stage_run_id: "exec-B".into(),
            nonce: 24,
            ..home_execution()
        }),
    );
    f.app.replace_input("B draft");
    socket::write_json_line(peer.get_mut(), &json!({"type":"delivered","scope":"conversation","id":"home","stage_run_id":"exec-A","nonce":23,"req_id":command["req_id"]})).await.unwrap();
    let (id, result) = delivery(&mut rx).await;
    assert!(result.is_ok());
    if let Some(receipt) = f.app.finish_delivery(id, result) {
        assert!(!receipt.clear_draft);
        f.live.show_receipt(receipt);
    }
    assert_eq!(f.app.input.text(), "B draft");
    assert_eq!(active.as_deref(), Some("B"));
    assert!(f.app.active_request);
    f.assert_unsent();
}

#[tokio::test]
async fn home_authoritative_identity_requires_current_generation_and_nonempty_active_turn() {
    let mut f = Fixture::new();
    f.app.home_run_id = Some("original".into());
    for (generation, turn, active, id, kind) in [
        (6, "B", Some("B"), "stale-generation", "home_run_identity"),
        (7, "A", Some("B"), "stale-turn", "home_run_identity"),
        (7, "B", None, "idle", "home_run_identity"),
        (7, "", Some(""), "empty-turn", "home_run_identity"),
        (7, "B", Some("B"), "", "home_run_identity"),
        (7, "B", Some("B"), "wrong-kind", "turn_started"),
    ] {
        apply_home_identity(
            &mut f.app,
            &json!({"type":kind,"generation":generation,"turn":turn,"id":id}),
            7,
            &active.map(str::to_owned),
        );
        assert_eq!(f.app.home_run_id.as_deref(), Some("original"));
    }
    apply_home_identity(
        &mut f.app,
        &json!({"type":"home_run_identity","generation":7,"turn":"B","id":"replacement"}),
        7,
        &Some("B".into()),
    );
    assert_eq!(f.app.home_run_id.as_deref(), Some("replacement"));
    f.assert_unsent();
}

#[tokio::test]
async fn home_background_stream_and_done_stay_in_home_without_summary_duplicate() {
    let mut f = Fixture::new();
    f.start_home("A");
    f.app.replace_input("Home draft");
    let mut active = Some("A".into());
    let other = Target::StageExecution {
        run_id: "other".into(),
        stage: "review".into(),
    };
    f.live.select_conversation(&mut f.app, other.clone());
    f.app.replace_input("Other draft");
    f.live.push_transcript_line("Other transcript".into());
    for event in [
        json!({"type":"reasoning_chunk","text":"private Home reasoning"}),
        json!({"type":"tool_output_chunk","id":"tool-1","name":"Bash","call":"echo home","text":"Home tool tail\n"}),
        json!({"type":"tool_result","id":"tool-1","name":"Bash","call":"echo home","output":"Home tool tail\n"}),
        json!({"type":"stream_chunk","text":"Home final"}),
        json!({"type":"turn_complete","turn":1,"text":"Home final answer","tool_calls":["echo home"]}),
    ] {
        f.home_event(&mut active, "A", event, Some(home_execution()));
        let visible = f.rendered();
        assert!(!visible.contains("Home tool tail"), "{visible}");
        assert!(!visible.contains("private Home reasoning"), "{visible}");
        assert_eq!(f.app.input.text(), "Other draft");
    }
    assert!(!f.rendered().contains("Home final"));
    f.home_event(
        &mut active,
        "A",
        json!({"type":"done","text":"Home final answer","summary":"DO NOT DUPLICATE SUMMARY"}),
        None,
    );
    assert!(active.is_none());
    assert_eq!(f.app.selected_target, other);
    assert_eq!(f.app.input.text(), "Other draft");
    let other_text = f.rendered();
    assert!(other_text.contains("Other transcript"));
    assert!(!other_text.contains("Home final answer"));
    f.live.select_conversation(&mut f.app, Target::Home);
    assert_eq!(f.app.input.text(), "Home draft");
    assert!(!f.app.active_request);
    assert!(!f.live.target_has_current_turn(&Target::Home));
    let home_text = f.rendered();
    assert_eq!(
        home_text.matches("Home final answer").count(),
        1,
        "{home_text}"
    );
    assert_eq!(
        home_text.matches("Home tool tail").count(),
        1,
        "{home_text}"
    );
    assert_eq!(home_text.matches("echo home").count(), 1, "{home_text}");
    assert!(!home_text.contains("private Home reasoning"));
    assert!(!home_text.contains("DO NOT DUPLICATE SUMMARY"));
    assert!(!home_text.contains("Other transcript"));
    f.assert_unsent();
}

#[tokio::test]
async fn home_replacement_missing_start_replay_renders_gap_and_preack_text_once() {
    let mut f = Fixture::with_socket();
    let (mut chat, mut raw) = f.connect_home_queues().await;
    install_home_session(&mut f.app, &f.client).await;
    assert_eq!(f.app.home_run_id.as_deref(), Some("home"));
    f.start_home("1");
    let mut active = Some("1".into());
    let client = Arc::clone(&f.client);
    let enqueue = tokio::spawn(async move {
        client
            .send_session_request_for_generation(
                json!({"op":"chat", "text":"replacement prompt", "turn":"1"}),
                0,
            )
            .await
            .unwrap()
    });
    let mut peer = f.peer().await;
    assert_eq!(
        read(&mut peer).await,
        json!({"op":"watch","scope":"run","id":"home","session_id":"test","replay":true})
    );
    for frame in [
        json!({"type":"watched","scope":"run","id":"home","stage":null,"stage_run_id":null,"replay":true}),
        json!({"type":"history_gap"}),
        json!({"type":"stream_chunk","text":"OLD RUN MUST NOT RENDER","run_id":"home","correlation_turn":"1","stage_type":"agent","stage_run_id":"old-exec"}),
        json!({"type":"replay_checkpoint"}),
    ] {
        socket::write_json_line(peer.get_mut(), &frame)
            .await
            .unwrap();
    }
    let mut session = BufReader::new(&mut f.peers[1]);
    let request = read(&mut session).await;
    assert_eq!(request["op"], "chat");
    assert_eq!(request["turn"], "1");
    // Replacement output predates this ack and is recovered by atomic replay.
    socket::write_json_line(
        session.get_mut(),
        &json!({"type":"ok","id":"replacement","session_id":"test"}),
    )
    .await
    .unwrap();
    let mut replacement = f.peer().await;
    assert_eq!(
        read(&mut replacement).await,
        json!({"op":"watch","scope":"run","id":"replacement","session_id":"test","replay":true})
    );
    let frames = [
        json!({"type":"watched","scope":"run","id":"replacement","stage":null,"stage_run_id":null,"replay":true}),
        json!({"type":"history_gap","run_id":"replacement"}),
        json!({"type":"stream_chunk","text":"OLD RUN MUST NOT RENDER","run_id":"home","correlation_turn":"old","stage_type":"agent","stage_run_id":"old-exec"}),
        json!({"type":"stream_chunk","text":"preack replacement text","run_id":"replacement","correlation_turn":"1","stage_type":"agent","stage_run_id":"replacement-exec"}),
        json!({"type":"replay_checkpoint"}),
        json!({"type":"done","text":"preack replacement text final","summary":"UNUSED SUMMARY","run_id":"replacement","correlation_turn":"1","stage_type":"agent","stage_run_id":"replacement-exec"}),
    ];
    let wire: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
    tokio::io::AsyncWriteExt::write_all(replacement.get_mut(), wire.as_bytes())
        .await
        .unwrap();
    assert_eq!(enqueue.await.unwrap()["id"], "replacement");
    let identity = tokio::time::timeout(Duration::from_secs(2), raw.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        identity,
        json!({"type":"home_run_identity", "id":"replacement",
        "turn":"1", "session_id":"test", "generation":0})
    );
    apply_home_identity(
        &mut f.app,
        &identity,
        f.client.session_generation().await,
        &active,
    );
    let mut kinds = Vec::new();
    while active.is_some() {
        let (event, turn, execution) = tokio::time::timeout(Duration::from_secs(2), chat.recv())
            .await
            .unwrap()
            .unwrap();
        kinds.push(
            serde_json::to_value(&event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        apply_home_event(
            &mut f.app,
            &mut f.live,
            &mut active,
            &mut HomeInterrupt::default(),
            event,
            turn,
            execution,
        )
        .unwrap();
        if kinds.last().map(String::as_str) == Some("system_warning") {
            assert!(f.app.active_request); // A display gap is not turn failure.
            assert_eq!(active.as_deref(), Some("1"));
        }
    }
    assert_eq!(kinds, ["system_warning", "stream_chunk", "done"]);
    assert!(chat.try_recv().is_err());
    let rendered = f.rendered();
    assert_eq!(
        rendered
            .matches("home display history discontinuity")
            .count(),
        1,
        "{rendered}"
    );
    assert_eq!(
        rendered.matches("preack replacement text final").count(),
        1,
        "{rendered}"
    );
    assert_eq!(
        rendered.matches("preack replacement text").count(),
        1,
        "{rendered}"
    );
    assert!(!rendered.contains("OLD RUN MUST NOT RENDER"));
    assert!(!rendered.contains("UNUSED SUMMARY"));
    assert_eq!(f.app.home_run_id.as_deref(), Some("replacement"));
    assert!(active.is_none());
    assert!(!f.app.active_request);
    f.assert_no_fallback().await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_successful_enqueue_then_prestage_failure_allows_fresh_untargeted_chat() {
    let mut f = Fixture::with_watch_socket(2);
    let (tx, _rx) = mpsc::unbounded_channel();
    f.start_home("A");
    let mut active = Some("A".into());
    // Record a successful enqueue independently of display output.
    let client = Arc::clone(&f.client);
    let enqueue = tokio::spawn(async move {
        client
            .send_session_request_for_generation(
                json!({"op":"chat","text":"first prompt","turn":"A"}),
                0,
            )
            .await
            .unwrap()
    });
    let mut observer = f.peer().await;
    assert_eq!(read(&mut observer).await["op"], "watch");
    socket::write_json_line(observer.get_mut(), &json!({"type":"watched","scope":"run","id":"home","stage":null,"stage_run_id":null,"replay":true})).await.unwrap();
    socket::write_json_line(observer.get_mut(), &json!({"type":"replay_checkpoint"}))
        .await
        .unwrap();
    let mut session = BufReader::new(&mut f.peers[1]);
    let first = read(&mut session).await;
    assert_eq!(first["op"], "chat");
    assert!(first.get("target").is_none());
    socket::write_json_line(
        session.get_mut(),
        &json!({"type":"ok","id":"home","session_id":"test"}),
    )
    .await
    .unwrap();
    assert_eq!(enqueue.await.unwrap()["type"], "ok");
    assert!(f.app.active_request);
    f.home_bus(
        &mut active,
        json!({"type":"error","message":"pre-stage setup failed",
        "run_id":"home","correlation_turn":"A"}),
    );
    assert!(active.is_none());
    assert!(!f.app.active_request);
    assert!(f.rendered().contains("pre-stage setup failed"));
    f.app.clear_input();
    f.app.replace_input("fresh untargeted prompt");
    assert!(!f.submit(&tx, false));
    let input = f.app.input.text().to_owned();
    f.app.clear_input();
    let (model_tx, _model_rx) = mpsc::unbounded_channel();
    let mut generation = 1;
    send_chat_input(
        &mut f.live,
        &mut f.app,
        &f.client,
        input,
        &mut generation,
        &mut active,
        0,
        model_tx,
    )
    .unwrap();
    let mut next_observer = f.peer().await;
    assert_eq!(read(&mut next_observer).await["op"], "watch");
    socket::write_json_line(next_observer.get_mut(), &json!({"type":"watched","scope":"run","id":"home","stage":null,"stage_run_id":null,"replay":true})).await.unwrap();
    socket::write_json_line(
        next_observer.get_mut(),
        &json!({"type":"replay_checkpoint"}),
    )
    .await
    .unwrap();
    let mut session = BufReader::new(&mut f.peers[1]);
    let fresh = read(&mut session).await;
    assert_eq!(fresh["op"], "chat");
    assert_eq!(fresh["text"], "fresh untargeted prompt");
    assert_eq!(fresh["turn"], "2");
    assert!(fresh.get("target").is_none());
    assert!(fresh.get("stage_run_id").is_none());
    socket::write_json_line(
        session.get_mut(),
        &json!({"type":"error","message":"fixture complete"}),
    )
    .await
    .unwrap();
    f.assert_no_fallback().await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_native_input_rejection_and_delivery_are_not_bus_responses() {
    let mut f = Fixture::with_watch_socket(2);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut active = Some("A".into());
    f.start_home("A");
    // A missing start frame, nonce, and capabilities must not prevent exact input.
    f.home_bus(
        &mut active,
        json!({"type":"reasoning_chunk","text":"native reasoning",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}),
    );
    f.app.replace_input("  native correction\n");
    assert!(f.submit(&tx, true));
    let (mut peer, request) = f.native_input_peer("home", "native-A").await;
    socket::write_json_line(
        peer.get_mut(),
        &json!({"type":"error","message":"agent is not ready"}),
    )
    .await
    .unwrap();
    let (id, result) = delivery(&mut rx).await;
    assert_eq!(id.to_string(), request["req_id"].as_str().unwrap());
    assert!(result.as_ref().unwrap_err().contains("agent is not ready"));
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert_eq!(f.app.input.text(), "  native correction\n");
    assert!(f.rendered().contains("agent is not ready"));
    f.assert_unsent();
    assert!(self::socket::read_json_line(&mut peer)
        .await
        .unwrap()
        .is_none());

    // Only another Enter retries. The receipt clears the draft, not the turn.
    assert!(f.submit(&tx, true));
    let (mut peer, request) = f.native_input_peer("home", "native-A").await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound","id":"home","stage_run_id":"native-A","req_id":request["req_id"]})).await.unwrap();
    assert_eq!(
        read(&mut peer).await,
        json!({"cmd":"message","req_id":request["req_id"],"text":"  native correction\n"})
    );
    socket::write_json_line(
        peer.get_mut(),
        &json!({"type":"delivered","req_id":request["req_id"]}),
    )
    .await
    .unwrap();
    let (id, result) = delivery(&mut rx).await;
    assert!(result.is_ok());
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert!(f.app.input.is_empty());
    assert!(f.app.active_request);
    assert_eq!(active.as_deref(), Some("A"));
    assert!(!f.rendered().contains("agent response after delivery"));
    f.home_bus(
        &mut active,
        json!({"type":"stream_chunk","text":"agent response after delivery",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}),
    );
    f.home_bus(
        &mut active,
        json!({"type":"done","text":"agent response after delivery","summary":"unused",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}),
    );
    assert_eq!(
        f.rendered()
            .matches("agent response after delivery")
            .count(),
        1
    );
    assert!(!f.app.active_request);
    f.assert_no_fallback().await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_queued_a_non_delivery_preserves_identity_after_normal_b_submission() {
    let mut f = Fixture::with_socket();
    let (mut chat, mut raw) = f.connect_home_queues().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut active = Some("A".into());
    f.start_home("A");
    f.home_bus(
        &mut active,
        json!({"type":"stream_chunk","text":"A output",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}),
    );
    f.app.replace_input("A correction kept for recovery");
    assert!(f.submit(&tx, true));
    let (mut peer, request) = f.native_input_peer("home", "native-A").await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound","id":"home","stage_run_id":"native-A","req_id":request["req_id"]})).await.unwrap();
    assert_eq!(
        read(&mut peer).await,
        json!({"cmd":"message","req_id":request["req_id"],"text":"A correction kept for recovery"})
    );
    socket::write_json_line(
        peer.get_mut(),
        &json!({"type":"queued","req_id":request["req_id"]}),
    )
    .await
    .unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(30), rx.recv())
        .await
        .is_err());
    f.home_event(
        &mut active,
        "A",
        json!({"type":"done","text":"A final","summary":""}),
        None,
    );
    f.app.clear_input(); // Explicit dismissal releases A's composer, not its receipt.
    f.app.replace_input("normal B prompt");
    assert!(!f.submit(&tx, false));
    // Exercise the normal prompt entry point, rather than manufacturing begin_home_turn.
    let (model_tx, _model_rx) = mpsc::unbounded_channel();
    let mut generation = 1;
    let input = f.app.input.text().to_owned();
    f.app.clear_input();
    send_chat_input(
        &mut f.live,
        &mut f.app,
        &f.client,
        input,
        &mut generation,
        &mut active,
        0,
        model_tx,
    )
    .unwrap();
    let mut observer = f.peer().await;
    assert_eq!(
        read(&mut observer).await,
        json!({"op":"watch","scope":"run","id":"home","session_id":"test","replay":true})
    );
    socket::write_json_line(observer.get_mut(), &json!({"type":"watched","scope":"run","id":"home","stage":null,"stage_run_id":null,"replay":true})).await.unwrap();
    socket::write_json_line(observer.get_mut(), &json!({"type":"replay_checkpoint"}))
        .await
        .unwrap();
    let mut session = BufReader::new(&mut f.peers[1]);
    let b = read(&mut session).await;
    assert_eq!(b["op"], "chat");
    assert_eq!(b["text"], "normal B prompt");
    assert_eq!(b["turn"], "2");
    assert!(b.get("target").is_none());
    socket::write_json_line(
        session.get_mut(),
        &json!({"type":"ok","id":"home","session_id":"test","status":"streaming"}),
    )
    .await
    .unwrap();
    assert_eq!(read(&mut session).await["op"], "chat_model");
    socket::write_json_line(session.get_mut(), &json!({"type":"ok","model":null}))
        .await
        .unwrap();
    drop(session);
    let identity = tokio::time::timeout(Duration::from_secs(2), raw.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(identity["type"], "home_run_identity");
    assert_eq!(identity["turn"], "2");
    apply_home_identity(
        &mut f.app,
        &identity,
        f.client.session_generation().await,
        &active,
    );
    socket::write_json_line(
        observer.get_mut(),
        &json!({"type":"stream_chunk","text":"B successfully started",
        "run_id":"home","stage_run_id":"native-B","stage_type":"agent","correlation_turn":"2"}),
    )
    .await
    .unwrap();
    let (event, turn, execution) = tokio::time::timeout(Duration::from_secs(2), chat.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.as_ref().unwrap().stage_run_id, "native-B");
    apply_home_event(
        &mut f.app,
        &mut f.live,
        &mut active,
        &mut HomeInterrupt::default(),
        event,
        turn,
        execution,
    )
    .unwrap();
    assert!(f.app.active_request);
    assert!(f.rendered().contains("B successfully started"));
    f.app.replace_input("B draft must survive");
    socket::write_json_line(
        peer.get_mut(),
        &json!({"type":"dropped","req_id":request["req_id"]}),
    )
    .await
    .unwrap();
    let (id, result) = delivery(&mut rx).await;
    assert!(result.is_err());
    let receipt = f.app.finish_delivery(id, result).unwrap();
    assert!(!receipt.clear_draft);
    assert!(receipt.text.contains("A correction kept for recovery"));
    assert!(receipt.text.contains("home/native-A"));
    f.live.show_receipt(receipt);
    assert_eq!(active.as_deref(), Some("2"));
    assert_eq!(f.app.input.text(), "B draft must survive");
    let rendered = f.rendered();
    assert!(
        rendered.contains("A correction kept for recovery"),
        "{rendered}"
    );
    assert!(rendered.contains("native-A"), "{rendered}");
    assert!(f.app.active_request);
    assert!(!rendered.contains("B setup pending"));
    assert!(rx.try_recv().is_err());
    assert!(chat.try_recv().is_err());
    // No automatic retry to B's now-live execution, nor a fallback chat.
    let mut session = BufReader::new(&mut f.peers[1]);
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        socket::read_json_line(&mut session)
    )
    .await
    .is_err());
    f.assert_no_fallback().await;
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

async fn read<S: tokio::io::AsyncRead + Unpin>(peer: &mut BufReader<S>) -> Value {
    tokio::time::timeout(Duration::from_secs(2), socket::read_json_line(peer))
        .await
        .expect("socket request timed out")
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn debug_leaving_before_ready_ignores_buffered_late_ready() {
    let mut f = Fixture::with_socket();
    let mut peer = f.attach("debugger", "exec").await;
    let target = f.app.selected_target.clone();
    f.receive(&mut peer, json!({"type":"waiting_for_ready"}))
        .await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"ready"}))
        .await
        .unwrap();
    let late = tokio::time::timeout(Duration::from_secs(2), f.rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(f.control("/exit"));
    let rendered = f.rendered();
    apply_attachment_frame(&mut f.app, &mut f.live, &mut f.attachment, late);
    assert_eq!(f.app.selected_target, Target::Home);
    assert_ne!(
        f.app.conversation_status(&target),
        ConversationStatus::Paused
    );
    assert_eq!(f.rendered(), rendered);
    f.assert_closed(&mut peer).await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn debug_uses_aux_mode_identity_controls_and_detach() {
    let mut f = Fixture::with_socket();
    for mode in ["debugger"] {
        let mut peer = f.attach(mode, "exec").await;
        let execution = f.attachment.execution.as_ref().unwrap();
        assert_eq!(execution.run_id, "run");
        assert_eq!(execution.stage_run_id, "exec");
        assert_eq!(execution.nonce, 9);
        assert_eq!(
            f.app.conversation_status(&f.app.selected_target),
            ConversationStatus::Running
        );
        if mode == "debugger" {
            f.receive(&mut peer, json!({"type":"ready"})).await;
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

        assert!(f.control("/exit"));
        assert_eq!(f.app.selected_target, Target::Home);
        f.assert_closed(&mut peer).await;
    }
    f.client.shutdown().await;
}
