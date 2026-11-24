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
