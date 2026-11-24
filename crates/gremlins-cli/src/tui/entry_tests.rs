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
            "control" => self.watch("run"),
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
async fn watch_home_preserves_idle_and_streaming_drafts_without_attachment_or_send() {
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
async fn watch_partial_correction_then_exit_closes_without_execution_control() {
    let mut f = Fixture::with_socket();
    install_home_session(&mut f.app, &f.client).await;
    f.app.replace_input("home draft");
    let mut peer = f.attach("control", "original").await;
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
async fn rapid_watch_home_watch_rejects_buffered_old_generation_for_same_execution() {
    let mut f = Fixture::with_socket();
    install_home_session(&mut f.app, &f.client).await;
    let mut first = f.attach("control", "same-execution").await;
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
    let mut current = f.attach("control", "same-execution").await;
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
async fn watch_and_debug_use_aux_modes_identity_controls_and_detach() {
    let mut f = Fixture::with_socket();
    for mode in ["control", "debugger"] {
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
