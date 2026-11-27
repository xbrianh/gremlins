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

    fn attach(&mut self, run_id: String, stage: Option<String>) {
        attach_stage_entry(
            &mut self.app,
            &mut self.live,
            &self.client,
            run_id,
            stage,
            &mut self.attachment,
            &self.tx,
            &mut self.last_stage,
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

    async fn receive(&mut self, peer: &mut BufReader<UnixStream>, value: Value) {
        reply(peer, value).await;
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
    for (run_id, expected) in [
        ("home", Target::Home),
        (
            "test",
            Target::StageExecution {
                run_id: "test".into(),
                stage: String::new(),
            },
        ),
    ] {
        f.attach(run_id.into(), None);
        assert_eq!(f.app.selected_target, expected);
        assert_eq!(f.attachment.task.is_none(), expected.is_home());
        f.attachment.invalidate(&mut f.app);
    }
    let (replacement, _peers) = client::DaemonClient::fake_with_identity(
        json!({"session_id":"sess-other", "home_run_id":"chat-reserved"}),
    );
    install_home_session(&mut f.app, &replacement).await;
    assert_eq!(f.app.home_run_id.as_deref(), Some("chat-reserved"));
    for (run_id, expected) in [
        ("chat-reserved", Target::Home),
        (
            "home",
            Target::StageExecution {
                run_id: "home".into(),
                stage: String::new(),
            },
        ),
    ] {
        f.attach(run_id.into(), None);
        assert_eq!(f.app.selected_target, expected);
        assert_eq!(f.attachment.task.is_none(), expected.is_home());
        f.attachment.invalidate(&mut f.app);
    }
    f.assert_unsent();
    replacement.shutdown().await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn home_selection_without_run_is_passive() {
    let mut f = Fixture::new();
    select_passive_watch(
        &mut f.app,
        &mut f.live,
        &f.client,
        Target::Home,
        &mut f.attachment,
        &f.tx,
        &mut f.last_stage,
    );
    assert_eq!(f.app.selected_target, Target::Home);
    assert!(f.attachment.task.is_none());
    f.assert_unsent();
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
        f.app.conversations.submit(&target, &input),
        Submission::Correction { .. }
    ));
    for command in ["/continue", "/bail stop"] {
        assert!(f.control(command));
    }
    assert!(!f.control("/quit"));
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
    reply(
        &mut inline,
        json!({"type":"replay_checkpoint", "id":"run", "complete":true}),
    )
    .await;
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
        f.attach("home".into(), None);
        assert_eq!(f.app.selected_target, Target::Home);
        assert_eq!(f.app.screen, Screen::Chat);
        assert_eq!(f.app.input.text(), draft);
        assert_eq!(f.app.active_request, streaming);
        if streaming {
            assert_eq!(f.live.response_text(), "home streaming response");
        }
        assert!(f.attachment.target.is_none());
        assert!(f.attachment.task.is_none());
        f.assert_unsent();
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
async fn attach_unresolved_home_stage_is_view_only_without_input_or_chat_fallback() {
    let mut f = Fixture::with_watch_socket(1);
    install_home_session(&mut f.app, &f.client).await;
    f.attach("home".into(), Some("chat".into()));
    let target = Target::StageExecution {
        run_id: "home".into(),
        stage: "chat".into(),
    };
    assert_eq!(f.app.selected_target, target);
    assert_eq!(f.app.screen, Screen::Chat);
    assert_eq!(
        f.app.conversation_status(&target),
        ConversationStatus::ReadOnly
    );
    assert!(f.attachment.execution.is_none());
    assert!(f.attachment.task.is_some());
    let mut peer = f.peer().await;
    assert_eq!(
        read(&mut peer).await,
        json!({"op":"watch", "scope":"run", "id":"home", "stage":"chat", "session_id":"test", "replay":true})
    );
    f.receive(&mut peer, json!({"type":"watched", "scope":"run", "id":"home", "stage":"chat", "stage_run_id":null, "replay":true})).await;
    assert_eq!(f.app.selected_target, target);
    assert!(f.attachment.execution.is_none());
    assert!(f.rendered().contains("view-only selection"));
    f.app.replace_input("not a home chat prompt");
    let (tx, mut rx) = mpsc::unbounded_channel();
    assert!(f.submit(&tx));
    assert!(rx.try_recv().is_err());
    assert_eq!(f.app.input.text(), "not a home chat prompt");
    f.assert_unsent();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), socket::read_json_line(&mut peer))
            .await
            .is_err(),
        "view-only submission must not send input on the watch socket"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            f.socket.as_ref().unwrap().0.accept()
        )
        .await
        .is_err(),
        "view-only submission must not open an input socket"
    );
    f.attachment.invalidate(&mut f.app);
    f.assert_closed(&mut peer).await;
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
    f.attach(run_id, stage);
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
    assert!(f.attachment.execution.is_none());
    for command in ["/continue", "/bail stop"] {
        assert!(f.control(command));
    }
    assert!(!f.control("/quit"));
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

    fn home_output(&mut self, turn: &str, text: &str, execution: protocol::Execution) {
        self.home_event(
            turn,
            json!({"type":"stream_chunk","text":text}),
            Some(execution),
        );
    }

    fn home_event(&mut self, turn: &str, value: Value, execution: Option<protocol::Execution>) {
        apply_home_event(
            &mut self.app,
            &mut self.live,
            &mut HomeInterrupt::default(),
            serde_json::from_value(value).unwrap(),
            Some(turn.into()),
            execution,
        )
        .unwrap();
    }

    fn submit(&mut self, tx: &mpsc::UnboundedSender<(u64, Result<(), String>)>) -> bool {
        route_submission(
            &mut self.app,
            &mut self.live,
            &self.client,
            &self.attachment,
            tx,
        )
    }

    fn send_prompt(&mut self, generation: &mut u64) {
        let (model_tx, _model_rx) = mpsc::unbounded_channel();
        let input = self.app.take_input().resolve();
        send_chat_input(
            &mut self.live,
            &mut self.app,
            &self.client,
            input,
            generation,
            0,
            model_tx,
        )
        .unwrap();
    }

    // Native bus frames have correlation_turn (not EventKind's numeric turn).
    fn home_bus(&mut self, frame: Value) {
        let turn = frame["correlation_turn"].as_str().unwrap().to_owned();
        let execution = (frame["stage_type"] == "agent")
            .then(|| protocol::Execution::from_bus_frame(&frame))
            .flatten();
        self.home_event(&turn, frame, execution);
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

    // Live-only preenqueue watches need only an ack; recovery replays history.
    async fn home_peer(&self, id: &str, replay: bool) -> BufReader<UnixStream> {
        let mut peer = self.peer().await;
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"watch","scope":"run","id":id,"session_id":"test","replay":replay})
        );
        socket::write_json_line(peer.get_mut(), &json!({"type":"watched","scope":"run","id":id,"stage":null,"stage_run_id":null,"replay":replay})).await.unwrap();
        peer
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

    async fn correction_peer(&self) -> (BufReader<UnixStream>, Value) {
        let mut peer = self.peer().await;
        let request = read(&mut peer).await;
        assert_eq!(
            request,
            json!({"op":"input", "session_id":"test", "id":"home", "stage_run_id":"exec-A", "nonce":23, "req_id":request["req_id"]})
        );
        assert!(request["req_id"].as_str().unwrap().parse::<u64>().is_ok());
        (peer, request)
    }
}

fn home_execution() -> protocol::Execution {
    protocol::Execution {
        run_id: "home".into(),
        stage_run_id: "exec-A".into(),
        nonce: 23,
    }
}

// Server-side lifecycle replies share the same checked JSON-line transport.
async fn reply<W: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    peer: &mut BufReader<W>,
    frame: Value,
) {
    socket::write_json_line(peer.get_mut(), &frame)
        .await
        .unwrap();
}

async fn input_status(peer: &mut BufReader<UnixStream>, kind: &str, request: &Value) {
    reply(peer, json!({"type":kind, "req_id":request["req_id"]})).await;
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
async fn home_stale_task_progress_cannot_render_or_rebind_replacement() {
    let mut f = Fixture::new();
    f.start_home("B");
    f.app.replace_input("B draft");
    let before = f.rendered();
    f.home_event(
        "A",
        json!({
            "type":"task_progress", "parent_id":"stale-task", "chain":"a.b",
            "kind":"Scout", "call":"STALE task", "reasoning":"STALE reasoning", "text":"STALE text"
        }),
        Some(home_execution()),
    );
    assert_eq!(f.rendered(), before);
    assert_eq!(f.app.conversations.home_turn(), Some("B"));
    assert!(f.app.active_request);
    assert_eq!(f.app.input.text(), "B draft");
    assert!(matches!(
        f.app.conversations.submit(&Target::Home, &f.app.input),
        Submission::Blocked { .. }
    ));
    f.assert_unsent();
}

#[tokio::test]
async fn home_queued_draft_waits_for_any_agent_execution_then_delivers_exact_correction() {
    let mut f = Fixture::with_watch_socket(1);
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.start_home("A");
    f.app.replace_input("  stop here\n");
    assert!(f.submit(&tx));
    f.assert_unsent();
    assert!(rx.try_recv().is_err());
    assert_eq!(f.app.input.text(), "  stop here\n");
    assert!(tokio::time::timeout(Duration::from_millis(30), rx.recv())
        .await
        .is_err());

    // No TurnStarted or capabilities: an ordinary agent frame carries identity.
    f.home_output("A", "working", home_execution());
    assert!(f.submit(&tx));
    assert!(f.app.input.is_empty());
    let (mut peer, request) = f.correction_peer().await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound","id":"home","stage_run_id":"exec-A","req_id":request["req_id"]})).await.unwrap();
    let command = read(&mut peer).await;
    assert_eq!(
        command,
        json!({"cmd":"message","req_id":request["req_id"],"text":"  stop here\n"})
    );
    assert!(command["req_id"].as_str().unwrap().parse::<u64>().is_ok());
    socket::write_json_line(peer.get_mut(), &json!({"type":"delivered","scope":"conversation","id":"home","stage_run_id":"exec-A","nonce":23,"req_id":command["req_id"]})).await.unwrap();
    let (id, result) = delivery(&mut rx).await;
    assert_eq!(id.to_string(), command["req_id"].as_str().unwrap());
    assert!(result.is_ok());
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert!(f.app.input.is_empty());
    f.client.release_inputs();
    f.assert_closed(&mut peer).await;
    f.assert_unsent();
}

#[tokio::test]
async fn home_rejected_corrections_recover_in_transcript_and_require_explicit_retry() {
    let mut f = Fixture::with_watch_socket(3);
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.start_home("A");
    f.home_event(
        "A",
        json!({"type":"reasoning_chunk","text":"thinking"}),
        Some(home_execution()),
    );
    for reason in ["busy", "unsupported", "not_ready"] {
        f.app.replace_input("keep this correction");
        assert!(f.submit(&tx));
        assert!(f.app.input.is_empty());
        let (mut peer, _) = f.correction_peer().await;
        reply(&mut peer, json!({"type":"error","message":reason})).await;
        let (id, result) = delivery(&mut rx).await;
        assert!(result.as_ref().unwrap_err().contains(reason));
        f.live
            .show_receipt(f.app.finish_delivery(id, result).unwrap());
        assert!(f.app.input.is_empty());
        assert!(f.rendered().contains("keep this correction"));
        f.client.release_inputs();
        f.assert_closed(&mut peer).await;
        assert!(rx.try_recv().is_err());
        assert!(tokio::time::timeout(Duration::from_millis(30), rx.recv())
            .await
            .is_err());
        f.assert_unsent();
    }
}

#[tokio::test]
async fn home_late_terminals_cannot_finish_or_rebind_active_replacement() {
    let mut f = Fixture::new();
    f.start_home("B");
    let b = protocol::Execution {
        stage_run_id: "exec-B".into(),
        nonce: 24,
        ..home_execution()
    };
    f.home_output("B", "B response", b.clone());
    f.app.replace_input("B draft");
    for stale in [
        json!({"type":"done","text":"STALE","summary":"STALE summary"}),
        json!({"type":"ended","reason":"STALE"}),
        json!({"type":"error","message":"STALE"}),
    ] {
        f.home_event("A", stale, Some(home_execution()));
        assert_eq!(f.app.conversations.home_turn(), Some("B"));
        assert!(f.app.active_request);
        assert_eq!(f.app.input.text(), "B draft");
        assert!(f.live.target_has_current_turn(&Target::Home));
        assert!(!f.rendered().contains("STALE"));
    }
    assert!(
        matches!(f.app.conversations.submit(&Target::Home, &f.app.input), Submission::Correction { execution, .. } if execution == b)
    );
    f.assert_unsent();
}

#[tokio::test]
async fn home_events_require_nonempty_matching_turn_before_any_mutation() {
    for active_id in [Some("B"), Some(""), None] {
        for event_turn in [None, Some(""), Some("A")] {
            let mut f = Fixture::new();
            f.start_home(active_id.unwrap_or(""));
            if active_id.is_none() {
                f.app.conversations.retire_home_turn();
            }
            f.app.replace_input("queued correction");
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
                    &mut interrupt,
                    serde_json::from_value(event).unwrap(),
                    event_turn.map(str::to_owned),
                    Some(home_execution()),
                )
                .unwrap();
                assert_eq!(f.app.conversations.home_turn(), active_id);
                assert!(f.app.active_request);
                assert!(f.live.target_has_current_turn(&Target::Home));
                assert_eq!(f.app.input.text(), "queued correction");
                assert!(!f.rendered().contains("UNBOUND"));
                let submitted = f.app.conversations.submit(&Target::Home, &f.app.input);
                if active_id.is_none() {
                    assert_eq!(submitted, Submission::HomePrompt);
                } else {
                    assert!(matches!(submitted, Submission::Blocked(_)));
                }
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
        if !chunks.is_empty() {
            f.home_event("A", json!({"type":"tool_output_chunk","id":"read","name":"Read","call":"Read(x)","text":chunks}), None);
        }
        for _ in 0..2 {
            f.home_event("A", json!({"type":"tool_result","id":"read","name":"Read","call":"Read(x)","output":"first\nmissing\nlast\n"}), None);
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
    let mut f = Fixture::with_watch_socket(1);
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.start_home("A");
    f.home_output("A", "A", home_execution());
    f.app.replace_input("A correction");
    assert!(f.submit(&tx));
    let (mut peer, request) = f.correction_peer().await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound","id":"home","stage_run_id":"exec-A","req_id":request["req_id"]})).await.unwrap();
    let command = read(&mut peer).await;
    assert_eq!(
        command,
        json!({"cmd":"message","req_id":request["req_id"],"text":"A correction"})
    );
    f.home_event(
        "A",
        json!({"type":"done","text":"A final","summary":""}),
        None,
    );
    f.start_home("B");
    f.home_event(
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
        f.live.show_receipt(receipt);
    }
    assert_eq!(f.app.input.text(), "B draft");
    assert_eq!(f.app.conversations.home_turn(), Some("B"));
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
        f.app.conversations.retire_home_session();
        if let Some(active) = active {
            f.app.conversations.begin_home_turn(active.into());
        }
        apply_home_identity(
            &mut f.app,
            &json!({"type":kind,"generation":generation,"turn":turn,"id":id}),
            7,
        );
        assert_eq!(f.app.home_run_id.as_deref(), Some("original"));
    }
    for (turn, retired, ack, id, expected) in [
        ("A", false, "A", "first", "first"),
        ("B", false, "A", "stale", "first"),
        ("B", false, "B", "replacement", "replacement"),
        ("B", true, "A", "stale", "replacement"),
        ("B", true, "B", "late", "late"),
    ] {
        f.app.conversations.begin_home_turn(turn.into());
        if retired {
            f.app.conversations.retire_home_turn();
        }
        apply_home_identity(
            &mut f.app,
            &json!({"type":"home_run_identity","generation":7,"turn":ack,"id":id}),
            7,
        );
        assert_eq!(f.app.home_run_id.as_deref(), Some(expected));
    }
    f.app.conversations.begin_home_turn("B".into());
    f.app.active_request = true;
    assert!(!f.live.target_has_current_turn(&Target::Home));
    let mut interrupt = HomeInterrupt::default();
    interrupt.press(Instant::now());
    for _ in 0..2 {
        apply_home_event(
            &mut f.app,
            &mut f.live,
            &mut interrupt,
            gremlins_magic_bus::EventKind::Ended {
                reason: "interrupted".into(),
            },
            Some("B".into()),
            None,
        )
        .unwrap();
        assert_eq!(f.app.conversations.home_turn(), None);
        assert!(!f.app.active_request);
        assert_eq!(interrupt.ended_turn.as_deref(), Some("B"));
        assert_eq!(f.rendered().matches("chat ended: interrupted").count(), 1);
    }
    f.assert_unsent();
}

#[tokio::test]
async fn home_background_stream_and_done_stay_in_home_without_summary_duplicate() {
    let mut f = Fixture::new();
    f.start_home("A");
    f.app.replace_input("Home draft");
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
        f.home_event("A", event, Some(home_execution()));
        let visible = f.rendered();
        assert!(!visible.contains("Home tool tail"), "{visible}");
        assert!(!visible.contains("private Home reasoning"), "{visible}");
        assert_eq!(f.app.input.text(), "Other draft");
    }
    assert!(!f.rendered().contains("Home final"));
    f.home_event(
        "A",
        json!({"type":"done","text":"Home final answer","summary":"DO NOT DUPLICATE SUMMARY"}),
        None,
    );
    assert!(f.app.conversations.home_turn().is_none());
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
async fn home_live_preenqueue_and_replacement_replay_render_early_output_once() {
    for replacement in [false, true] {
        let id = if replacement { "replacement" } else { "home" };
        let mut f = Fixture::with_socket();
        let (mut chat, mut raw) = f.connect_home_queues().await;
        install_home_session(&mut f.app, &f.client).await;
        assert_eq!(f.app.home_run_id.as_deref(), Some("home"));
        f.start_home("1");
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
        // Existing history may be incomplete: replay:false must enqueue without
        // receiving old history, a history_gap, or any replay checkpoint.
        let mut initial = f.home_peer("home", false).await;
        if !replacement {
            socket::write_json_line(initial.get_mut(), &json!({"type":"stream_chunk","text":"preack replacement text","run_id":"home","correlation_turn":"1","stage_type":"agent","stage_run_id":"early-exec"})).await.unwrap();
        }
        let mut session = BufReader::new(&mut f.peers[1]);
        let request = read(&mut session).await;
        assert_eq!(request["op"], "chat");
        assert_eq!(request["turn"], "1");
        // Replacement output predates this ack and is recovered by atomic replay.
        reply(
            &mut session,
            json!({"type":"ok","id":id,"session_id":"test"}),
        )
        .await;
        let mut output = if replacement {
            f.home_peer(id, true).await
        } else {
            initial
        };
        let mut frames = if replacement {
            vec![
                json!({"type":"history_gap","run_id":"replacement"}),
                json!({"type":"stream_chunk","text":"OLD RUN MUST NOT RENDER","run_id":"home","correlation_turn":"old","stage_type":"agent","stage_run_id":"old-exec"}),
                json!({"type":"stream_chunk","text":"preack replacement text","run_id":"replacement","correlation_turn":"1","stage_type":"agent","stage_run_id":"early-exec"}),
                json!({"type":"replay_checkpoint"}),
            ]
        } else {
            Vec::new()
        };
        frames.push(json!({"type":"done","text":"preack replacement text final","summary":"UNUSED SUMMARY","run_id":id,"correlation_turn":"1","stage_type":"agent","stage_run_id":"early-exec"}));
        let wire: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
        tokio::io::AsyncWriteExt::write_all(output.get_mut(), wire.as_bytes())
            .await
            .unwrap();
        assert_eq!(enqueue.await.unwrap()["id"], id);
        let identity = tokio::time::timeout(Duration::from_secs(2), raw.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            identity,
            json!({"type":"home_run_identity", "id":id,
        "turn":"1", "session_id":"test", "generation":0})
        );
        apply_home_identity(&mut f.app, &identity, f.client.session_generation().await);
        let mut kinds = Vec::new();
        while f.app.conversations.home_turn().is_some() {
            let (event, turn, execution) =
                tokio::time::timeout(Duration::from_secs(2), chat.recv())
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
                &mut HomeInterrupt::default(),
                event,
                turn,
                execution,
            )
            .unwrap();
            if kinds.last().map(String::as_str) == Some("system_warning") {
                assert!(f.app.active_request); // A display gap is not turn failure.
                assert_eq!(f.app.conversations.home_turn(), Some("1"));
            }
        }
        assert_eq!(
            kinds,
            if replacement {
                vec!["system_warning", "stream_chunk", "done"]
            } else {
                vec!["stream_chunk", "done"]
            }
        );
        assert!(chat.try_recv().is_err());
        let rendered = f.rendered();
        assert_eq!(
            rendered
                .matches("home display history discontinuity")
                .count(),
            usize::from(replacement),
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
        assert_eq!(f.app.home_run_id.as_deref(), Some(id));
        assert!(f.app.conversations.home_turn().is_none());
        assert!(!f.app.active_request);
        f.assert_no_fallback().await;
        f.client.shutdown().await;
    }
}

#[tokio::test]
async fn home_two_turn_completion_routes_parked_text_as_fresh_prompts() {
    for mode in [
        "visible",
        "parked",
        "missing-done",
        "missing-ended",
        "correction",
        "failure",
        "starting-correction",
        "enqueue-rejection",
    ] {
        let mut f = Fixture::with_socket();
        let (mut chat, _raw) = f.connect_home_queues().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut generation = 0;
        f.app.replace_input("first prompt");
        assert!(!f.submit(&tx));
        assert_eq!(f.app.input.text(), "first prompt");
        assert!(rx.try_recv().is_err());
        f.assert_unsent();
        f.send_prompt(&mut generation);
        let mut observer = f.home_peer("home", false).await;
        let mut session = BufReader::new(&mut f.peers[1]);
        let first = read(&mut session).await;
        assert_eq!(first["op"], "chat");
        assert_eq!(first["turn"], "1");
        assert!(first.get("target").is_none());
        assert_eq!(first["text"], "first prompt");
        assert_eq!(first["session_id"], "test");
        drop(session);
        if mode == "starting-correction" || mode == "enqueue-rejection" {
            f.app.replace_input("second prompt");
            assert!(f.submit(&tx));
            assert_eq!(f.app.input.text(), "second prompt");
            assert!(rx.try_recv().is_err());
            f.assert_no_fallback().await;
        }
        let mut session = BufReader::new(&mut f.peers[1]);
        if mode == "enqueue-rejection" {
            reply(
                &mut session,
                json!({"type":"error","message":"socket enqueue rejected"}),
            )
            .await;
        } else {
            reply(
                &mut session,
                json!({"type":"ok","id":"home","session_id":"test"}),
            )
            .await;
            assert_eq!(read(&mut session).await["op"], "chat_model");
            reply(&mut session, json!({"type":"ok","model":null})).await;
        }
        drop(session);
        if mode != "failure" && mode != "starting-correction" && mode != "enqueue-rejection" {
            f.home_event(
                "1",
                json!({"type":"reasoning_chunk","text":"working"}),
                Some(home_execution()),
            );
            if mode != "visible" && mode != "parked" {
                f.home_event("1", json!({"type":"tool_result","id":"read","name":"Read","call":"Read(x)","output":"final tool result"}), Some(home_execution()));
            }
        }
        if mode == "correction" || mode == "parked" {
            f.app.replace_input("second prompt");
        }
        if mode == "parked" {
            f.live.select_conversation(
                &mut f.app,
                Target::StageExecution {
                    run_id: "other".into(),
                    stage: "review".into(),
                },
            );
            f.app.replace_input("unrelated draft");
        }
        if mode.starts_with("missing") {
            f.live.clear_current_turn();
        }
        let terminal = if mode == "failure" || mode == "starting-correction" {
            json!({"type":"error","message":"pre-stage setup failed"})
        } else if mode == "missing-ended" {
            json!({"type":"ended","reason":"finished"})
        } else {
            json!({"type":"done","text":"first final","summary":""})
        };
        if mode == "visible" || mode == "parked" {
            for mut frame in [
                json!({"type":"tool_result","id":"read","name":"Read","call":"Read(x)","output":"final tool result"}),
                terminal,
            ] {
                frame["run_id"] = json!("home");
                frame["correlation_turn"] = json!("1");
                frame["stage_type"] = json!("agent");
                frame["stage_run_id"] = json!("exec-A");
                socket::write_json_line(observer.get_mut(), &frame)
                    .await
                    .unwrap();
                let (event, turn, execution) =
                    tokio::time::timeout(Duration::from_secs(2), chat.recv())
                        .await
                        .unwrap()
                        .unwrap();
                apply_home_event(
                    &mut f.app,
                    &mut f.live,
                    &mut HomeInterrupt::default(),
                    event,
                    turn,
                    execution,
                )
                .unwrap();
            }
        } else if mode == "enqueue-rejection" {
            let (event, turn, execution) =
                tokio::time::timeout(Duration::from_secs(2), chat.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(serde_json::to_value(&event).unwrap()["type"], "error");
            assert_eq!(turn.as_deref(), Some("1"));
            assert!(execution.is_none());
            apply_home_event(
                &mut f.app,
                &mut f.live,
                &mut HomeInterrupt::default(),
                event,
                turn,
                execution,
            )
            .unwrap();
            assert!(f.rendered().contains("socket enqueue rejected"));
            assert!(chat.try_recv().is_err());
        } else {
            f.home_event("1", terminal, None);
        }
        if mode == "parked" {
            assert_eq!(f.app.input.text(), "unrelated draft");
            f.live.select_conversation(&mut f.app, Target::Home);
        }
        assert!(
            f.app.conversations.home_turn().is_none(),
            "{mode}: terminal did not retire turn"
        );
        assert!(
            !f.app.active_request,
            "{mode}: terminal left request active"
        );
        let parked_text =
            mode.ends_with("correction") || mode == "enqueue-rejection" || mode == "parked";
        if parked_text {
            assert_eq!(f.app.input.text(), "second prompt");
        } else {
            assert!(f.app.input.is_empty(), "{mode}: composer was not consumed");
            f.app.replace_input("second prompt");
        }
        if !mode.starts_with("missing")
            && mode != "failure"
            && !mode.ends_with("correction")
            && mode != "enqueue-rejection"
        {
            assert!(f.rendered().contains(REASONING_COMPLETED_TITLE));
            assert!(f.rendered().contains("first final"));
            assert!(f.rendered().contains("final tool result"));
        }
        if mode == "failure" {
            assert!(f.rendered().contains("pre-stage setup failed"));
        }
        let prompt = "second prompt";
        assert!(!f.submit(&tx), "{mode}: fresh prompt refused");
        f.send_prompt(&mut generation);
        let _next = f.home_peer("home", false).await;
        let mut session = BufReader::new(&mut f.peers[1]);
        let second = read(&mut session).await;
        assert_eq!(second["op"], "chat");
        assert_eq!(second["text"], prompt);
        assert_eq!(second["turn"], "2");
        assert_ne!(second["turn"], first["turn"]);
        assert!(second.get("target").is_none());
        assert!(second.get("stage_run_id").is_none());
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"error","message":"fixture complete"}),
        )
        .await
        .unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(30),
            socket::read_json_line(&mut session)
        )
        .await
        .is_err());
        drop(session);
        for terminal in [
            json!({"type":"done","text":"STALE","summary":""}),
            json!({"type":"ended","reason":"STALE"}),
            json!({"type":"error","message":"STALE"}),
        ] {
            f.home_event("1", terminal, Some(home_execution()));
            assert_eq!(f.app.conversations.home_turn(), Some("2"));
        }
        f.app.replace_input("third prompt");
        assert!(f.submit(&tx));
        assert_eq!(f.app.input.text(), "third prompt");
        assert!(rx.try_recv().is_err());
        f.assert_no_fallback().await;
        f.client.shutdown().await;
    }
}

#[tokio::test]
async fn home_native_input_rejection_and_delivery_are_not_bus_responses() {
    let mut f = Fixture::with_watch_socket(2);
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.start_home("A");
    // A missing start frame, nonce, and capabilities must not prevent exact input.
    f.home_bus(json!({"type":"reasoning_chunk","text":"native reasoning",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}));
    f.app.replace_input("  native correction\n");
    assert!(f.submit(&tx));
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
    assert!(f.app.input.is_empty());
    assert!(f.rendered().contains("native correction"));
    assert!(f.rendered().contains("agent is not ready"));
    f.assert_unsent();
    assert!(self::socket::read_json_line(&mut peer)
        .await
        .unwrap()
        .is_none());

    // Recovery is in the transcript; explicitly re-enter text to retry.
    f.app.replace_input("  native correction\n");
    assert!(f.submit(&tx));
    assert!(f.app.input.is_empty());
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
    assert_eq!(f.app.conversations.home_turn(), Some("A"));
    assert!(!f.rendered().contains("agent response after delivery"));
    f.home_bus(
        json!({"type":"stream_chunk","text":"agent response after delivery",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}),
    );
    f.home_bus(
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
    f.start_home("A");
    f.home_bus(json!({"type":"stream_chunk","text":"A output",
        "run_id":"home","stage_run_id":"native-A","stage_type":"agent","correlation_turn":"A"}));
    f.app.replace_input("A correction kept for recovery");
    assert!(f.submit(&tx));
    let (mut peer, request) = f.native_input_peer("home", "native-A").await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound","id":"home","stage_run_id":"native-A","req_id":request["req_id"]})).await.unwrap();
    assert_eq!(
        read(&mut peer).await,
        json!({"cmd":"message","req_id":request["req_id"],"text":"A correction kept for recovery"})
    );
    input_status(&mut peer, "queued", &request).await;
    assert!(tokio::time::timeout(Duration::from_millis(30), rx.recv())
        .await
        .is_err());
    f.home_event(
        "A",
        json!({"type":"done","text":"A final","summary":""}),
        None,
    );
    assert!(f.app.input.is_empty());
    f.app.replace_input("normal B prompt");
    assert!(!f.submit(&tx));
    // Exercise the normal prompt entry point, rather than manufacturing begin_home_turn.
    let mut generation = 1;
    f.send_prompt(&mut generation);
    let mut observer = f.home_peer("home", false).await;
    let mut session = BufReader::new(&mut f.peers[1]);
    let b = read(&mut session).await;
    assert_eq!(b["op"], "chat");
    assert_eq!(b["text"], "normal B prompt");
    assert_eq!(b["turn"], "2");
    assert!(b.get("target").is_none());
    reply(
        &mut session,
        json!({"type":"ok","id":"home","session_id":"test","status":"streaming"}),
    )
    .await;
    assert_eq!(read(&mut session).await["op"], "chat_model");
    reply(&mut session, json!({"type":"ok","model":null})).await;
    drop(session);
    let identity = tokio::time::timeout(Duration::from_secs(2), raw.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(identity["type"], "home_run_identity");
    assert_eq!(identity["turn"], "2");
    apply_home_identity(&mut f.app, &identity, f.client.session_generation().await);
    reply(
        &mut observer,
        json!({"type":"stream_chunk","text":"B successfully started",
        "run_id":"home","stage_run_id":"native-B","stage_type":"agent","correlation_turn":"2"}),
    )
    .await;
    let (event, turn, execution) = tokio::time::timeout(Duration::from_secs(2), chat.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.as_ref().unwrap().stage_run_id, "native-B");
    apply_home_event(
        &mut f.app,
        &mut f.live,
        &mut HomeInterrupt::default(),
        event,
        turn,
        execution,
    )
    .unwrap();
    assert!(f.app.active_request);
    assert!(f.rendered().contains("B successfully started"));
    f.app.replace_input("B draft must survive");
    input_status(&mut peer, "dropped", &request).await;
    let (id, result) = delivery(&mut rx).await;
    assert!(result.is_err());
    let receipt = f.app.finish_delivery(id, result).unwrap();
    assert!(receipt.text.contains("A correction kept for recovery"));
    assert!(receipt.text.contains("home/native-A"));
    f.live.show_receipt(receipt);
    assert_eq!(f.app.conversations.home_turn(), Some("2"));
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

impl Fixture {
    fn siblings(&mut self, kind: &str) {
        for id in ["A", "B"] {
            self.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":id, "stage_type":kind, "active":true}));
        }
    }

    async fn exact(&mut self, id: &str) -> BufReader<UnixStream> {
        self.debug(&format!("run {id}"));
        let mut peer = self.peer().await;
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"watch", "scope":"run", "id":"run", "stage":"review", "stage_run_id":id, "session_id":"test", "replay":true})
        );
        self.receive(&mut peer, json!({"type":"watched", "scope":"run", "id":"run", "stage":"review", "stage_run_id":id, "replay":true})).await;
        peer
    }

    async fn attach_exact(&mut self, id: &str) -> BufReader<UnixStream> {
        let Some(commands::CommandResult::AttachStage { run_id, stage }) =
            commands::dispatch(&format!("/attach run review {id}"))
        else {
            panic!("expected attach command");
        };
        self.attach(run_id, stage);
        let mut peer = self.peer().await;
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"watch", "scope":"run", "id":"run", "stage":"review", "stage_run_id":id, "session_id":"test", "replay":true})
        );
        self.receive(&mut peer, json!({"type":"watched", "scope":"run", "id":"run", "stage":"review", "stage_run_id":id, "replay":true})).await;
        peer
    }

    async fn bind_selected(&self, id: &str) -> BufReader<UnixStream> {
        let mut peer = self.peer().await;
        let request = read(&mut peer).await;
        assert_eq!(
            request,
            json!({"op":"input", "session_id":"test", "id":"run", "stage_run_id":id, "req_id":request["req_id"]})
        );
        assert!(!request["req_id"].as_str().unwrap().is_empty());
        socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound", "id":"run", "stage_run_id":id, "req_id":request["req_id"]})).await.unwrap();
        peer
    }
}

#[tokio::test]
async fn attach_explicit_identity_never_selects_live_same_name_sibling() {
    let mut f = Fixture::with_watch_socket(2);
    f.siblings("agent");
    let _a = f.attach_exact("A").await;
    assert_eq!(f.attachment.execution.as_ref().unwrap().stage_run_id, "A");
    f.home();
    f.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"A", "stage_type":"agent", "active":false}));
    let _expired_a = f.attach_exact("A").await;
    assert!(
        matches!(&f.app.selected_target, Target::Invocation { stage_run_id, .. } if stage_run_id == "A")
    );
    assert!(f.attachment.execution.is_none());
    assert_eq!(
        f.app.conversation_status(&f.app.selected_target),
        ConversationStatus::ReadOnly
    );
    assert!(f.rendered().contains("requested invocation is unavailable"));
    assert!(!f.control("/quit"));
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn unsent_parked_a_draft_cannot_correct_reentered_b() {
    let mut f = Fixture::with_watch_socket(3);
    f.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"A", "stage_type":"agent", "active":true}));
    let _a = f.attach_exact("A").await;
    f.app.replace_input("UNSENT A correction");
    f.home();
    f.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"A", "stage_type":"agent", "active":false}));
    f.app.observe_pipeline_frame(&json!({"type":"stage_boundary", "run_id":"run", "stage":"review", "stage_run_id":"B", "stage_type":"agent", "active":true}));
    let mut b = f.attach_exact("B").await;
    assert!(f.app.input.is_empty());
    let _expired_a = f.attach_exact("A").await;
    assert_eq!(f.app.input.text(), "UNSENT A correction");
    let (tx, mut rx) = mpsc::unbounded_channel();
    assert!(f.submit(&tx));
    assert_eq!(f.app.input.text(), "UNSENT A correction");
    assert!(rx.try_recv().is_err());
    assert!(socket::read_json_line(&mut b).await.unwrap().is_none());
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        f.socket.as_ref().unwrap().0.accept()
    )
    .await
    .is_err());
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn returning_home_routes_parked_text_only_to_current_live_b() {
    let mut f = Fixture::with_watch_socket(2);
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.start_home("A");
    f.home_output("A", "A working", home_execution());
    f.app.replace_input("parked home text");
    f.siblings("agent");
    let _stage = f.exact("B").await;
    f.app.replace_input("stage draft");
    f.home_event(
        "A",
        json!({"type":"done","text":"A final","summary":""}),
        None,
    );
    f.app.conversations.begin_home_turn("B".into());
    f.app.active_request = true;
    f.home_output(
        "B",
        "B working",
        protocol::Execution {
            run_id: "home".into(),
            stage_run_id: "exec-B".into(),
            nonce: 0,
        },
    );
    assert_eq!(f.app.input.text(), "stage draft");
    f.home();
    assert_eq!(f.app.input.text(), "parked home text");
    assert!(f.submit(&tx));
    assert!(f.app.input.is_empty());
    let (mut peer, request) = f.native_input_peer("home", "exec-B").await;
    reply(&mut peer, json!({"type":"input_bound","id":"home","stage_run_id":"exec-B","req_id":request["req_id"]})).await;
    let command = read(&mut peer).await;
    assert_eq!(
        command,
        json!({"cmd":"message","req_id":request["req_id"],"text":"parked home text"})
    );
    input_status(&mut peer, "delivered", &request).await;
    let (id, result) = delivery(&mut rx).await;
    assert!(result.is_ok());
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert!(f.app.input.is_empty());
    assert!(rx.try_recv().is_err());
    f.assert_no_fallback().await;
    f.client.shutdown().await;
}

#[tokio::test]
async fn returning_home_routes_parked_text_to_still_live_a() {
    let mut f = Fixture::with_watch_socket(2);
    f.start_home("A");
    f.home_output("A", "working", home_execution());
    f.app.replace_input("parked home correction");
    f.siblings("agent");
    let _stage = f.exact("B").await;
    f.home();
    assert_eq!(f.app.input.text(), "parked home correction");
    let (tx, mut rx) = mpsc::unbounded_channel();
    assert!(f.submit(&tx));
    let (mut peer, request) = f.correction_peer().await;
    socket::write_json_line(peer.get_mut(), &json!({"type":"input_bound", "id":"home", "stage_run_id":"exec-A", "req_id":request["req_id"]})).await.unwrap();
    let command = read(&mut peer).await;
    assert_eq!(
        command,
        json!({"cmd":"message", "req_id":request["req_id"], "text":"parked home correction"})
    );
    input_status(&mut peer, "delivered", &command).await;
    let (id, result) = delivery(&mut rx).await;
    assert!(result.is_ok());
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert!(f.app.input.is_empty());
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn run_only_quit_falls_through_to_local_dispatch() {
    let mut f = Fixture::with_watch_socket(1);
    f.siblings("agent");
    f.attach("run".into(), None);
    assert!(!f.control("/quit"));
    assert_eq!(f.app.input.text(), "/quit");
    assert!(matches!(
        commands::dispatch(f.app.input.text()),
        Some(commands::CommandResult::Quit)
    ));
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn exact_debug_is_passive_first_input_is_a_and_bus_continues_once() {
    let mut f = Fixture::with_watch_socket(2);
    f.siblings("agent");
    let mut bus = f.exact("A").await;
    assert_eq!(f.attachment.execution.as_ref().unwrap().stage_run_id, "A");
    assert_eq!(
        f.app.conversation_status(&f.app.selected_target),
        ConversationStatus::Running
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), socket::read_json_line(&mut bus))
            .await
            .is_err()
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.app.replace_input("correct A");
    assert!(f.submit(&tx));
    let mut input = f.bind_selected("A").await;
    let command = read(&mut input).await;
    assert_eq!(
        command,
        json!({"cmd":"message", "req_id":command["req_id"], "text":"correct A"})
    );
    input_status(&mut input, "delivered", &command).await;
    let (id, result) = delivery(&mut rx).await;
    f.live
        .show_receipt(f.app.finish_delivery(id, result).unwrap());
    assert!(f.app.input.is_empty());
    for (id, sequence, text) in [
        ("B", 1, "sibling output"),
        ("A", 2, "bus keeps streaming"),
        ("A", 2, "bus keeps streaming"),
    ] {
        f.receive(&mut bus, json!({"type":"stream_chunk", "run_id":"run", "stage":"review", "stage_run_id":id, "sequence":sequence, "text":text})).await;
    }
    let rendered = f.rendered();
    assert!(!rendered.contains("sibling output"));
    assert_eq!(
        rendered.matches("bus keeps streaming").count(),
        1,
        "{rendered}"
    );
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn selected_rejections_recover_in_transcript_and_only_explicit_retry_binds_again() {
    let mut f = Fixture::with_watch_socket(4);
    f.siblings("agent");
    let _bus = f.exact("A").await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    for reason in ["busy", "unsupported", "wrong ack"] {
        f.app.replace_input("keep correction");
        assert!(f.submit(&tx));
        assert!(f.app.input.is_empty());
        let (mut peer, request) = f.native_input_peer("run", "A").await;
        let response = if reason == "wrong ack" {
            json!({"type":"input_bound", "id":"run", "stage_run_id":"B", "req_id":request["req_id"]})
        } else {
            json!({"type":"error", "message":reason})
        };
        reply(&mut peer, response).await;
        let (id, result) = delivery(&mut rx).await;
        assert!(result.is_err());
        f.live
            .show_receipt(f.app.finish_delivery(id, result).unwrap());
        assert!(f.app.input.is_empty());
        assert!(f.rendered().contains("keep correction"));
        assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        assert!(rx.try_recv().is_err());
        f.assert_unsent();
    }
    f.client.shutdown().await;
}

#[tokio::test]
async fn selected_exec_is_readonly_and_quit_outside_selection_is_local() {
    let mut f = Fixture::with_watch_socket(1);
    assert!(!f.control("/quit"));
    assert!(matches!(
        commands::dispatch("/quit"),
        Some(commands::CommandResult::Quit)
    ));
    f.siblings("exec");
    let _bus = f.exact("A").await;
    assert_eq!(
        f.app.conversation_status(&f.app.selected_target),
        ConversationStatus::ReadOnly
    );
    f.app.replace_input("do not send");
    let (tx, mut rx) = mpsc::unbounded_channel();
    assert!(f.submit(&tx));
    assert_eq!(f.app.input.text(), "do not send");
    assert!(f.control("/pause"));
    assert!(!f.control("/quit"));
    assert!(matches!(
        commands::dispatch(f.app.input.text()),
        Some(commands::CommandResult::Quit)
    ));
    assert!(rx.try_recv().is_err());
    f.assert_unsent();
    f.client.shutdown().await;
}

#[tokio::test]
async fn delayed_selected_a_delivery_preserves_new_a_and_b_drafts() {
    selected_receipt_preserves_buffers(true).await;
}

#[tokio::test]
async fn delayed_selected_a_failure_recovers_only_in_originating_transcript() {
    selected_receipt_preserves_buffers(false).await;
}

async fn selected_receipt_preserves_buffers(success: bool) {
    let mut f = Fixture::with_watch_socket(3);
    f.siblings("agent");
    let _a_bus = f.exact("A").await;
    let a = f.app.selected_target.clone();
    let (tx, mut rx) = mpsc::unbounded_channel();
    f.app.replace_input("old A correction");
    assert!(f.submit(&tx));
    assert!(f.app.input.is_empty());
    let mut input = f.bind_selected("A").await;
    let command = read(&mut input).await;
    input_status(&mut input, "queued", &command).await;
    f.app.replace_input("new A draft");
    assert!(f.submit(&tx));
    assert_eq!(f.app.input.text(), "new A draft");
    assert!(rx.try_recv().is_err());
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        socket::read_json_line(&mut input)
    )
    .await
    .is_err());
    let _b_bus = f.exact("B").await;
    f.app.replace_input("new B draft");
    let b_before = f.rendered();
    input_status(
        &mut input,
        if success { "delivered" } else { "dropped" },
        &command,
    )
    .await;
    let (id, result) = delivery(&mut rx).await;
    assert_eq!(result.is_ok(), success);
    let receipt = f.app.finish_delivery(id, result).unwrap();
    if !success {
        assert!(receipt.text.contains("old A correction"));
    }
    f.live.show_receipt(receipt);
    assert_eq!(f.app.input.text(), "new B draft");
    assert_eq!(f.rendered(), b_before);
    assert!(rx.try_recv().is_err());
    f.assert_unsent();
    assert!(tokio::time::timeout(
        Duration::from_millis(30),
        f.socket.as_ref().unwrap().0.accept()
    )
    .await
    .is_err());
    f.live.select_conversation(&mut f.app, a);
    assert_eq!(f.app.input.text(), "new A draft");
    if success {
        assert_eq!(f.rendered().matches("correction delivered").count(), 1);
    } else {
        assert!(f.rendered().contains("old A correction"));
    }
    f.client.shutdown().await;
}

#[tokio::test]
async fn queued_selected_disconnect_or_drop_is_not_delivery() {
    for disconnect in [false, true] {
        let mut f = Fixture::with_watch_socket(2);
        f.siblings("agent");
        let _bus = f.exact("A").await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        f.app.replace_input("recover this");
        assert!(f.submit(&tx));
        let mut input = f.bind_selected("A").await;
        let command = read(&mut input).await;
        input_status(&mut input, "queued", &command).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        assert!(f.app.input.is_empty());
        if !disconnect {
            input_status(&mut input, "dropped", &command).await;
        }
        drop(input);
        let (id, result) = delivery(&mut rx).await;
        assert!(result.is_err());
        f.live
            .show_receipt(f.app.finish_delivery(id, result).unwrap());
        assert!(f.app.input.is_empty());
        assert!(f.rendered().contains("recover this"));
        f.assert_unsent();
        f.client.shutdown().await;
    }
}

#[tokio::test]
async fn selected_controls_reuse_native_handle_and_leaving_paused_sends_nothing() {
    let mut f = Fixture::with_watch_socket(4);
    f.siblings("agent");
    let mut bus = f.exact("A").await;
    assert!(f.control("/pause"));
    let mut input = f.bind_selected("A").await;
    for (index, (text, cmd)) in [
        ("/pause", "pause"),
        ("/step", "step"),
        ("/continue", "step"),
        ("/resume-execution", "resume"),
        ("/quit", "resume"),
        ("/bail enough", "bail"),
        ("/quit stop now", "bail"),
    ]
    .into_iter()
    .enumerate()
    {
        if index != 0 {
            assert!(f.control(text));
        }
        let command = read(&mut input).await;
        if cmd == "bail" {
            assert_eq!(command["reason"], text.split_once(' ').unwrap().1);
        }
        assert_eq!(command["cmd"], cmd);
        assert!(command.get("id").is_none());
        input_status(&mut input, "delivered", &command).await;
        f.frame().await;
    }
    f.app
        .set_conversation_status(f.app.selected_target.clone(), ConversationStatus::Paused);
    assert!(f.control("/exit"));
    assert_eq!(f.app.selected_target, Target::Home);
    assert!(socket::read_json_line(&mut input).await.unwrap().is_none());
    f.assert_closed(&mut bus).await;
    let mut bus = f.exact("A").await;
    assert_eq!(
        f.app.conversation_status(&f.app.selected_target),
        ConversationStatus::Paused
    );
    assert_eq!(f.attachment.execution.as_ref().unwrap().stage_run_id, "A");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), socket::read_json_line(&mut bus))
            .await
            .is_err(),
        "reselecting a paused invocation must not send a control"
    );
    f.assert_unsent();
    assert!(f.control("/resume-execution"));
    let mut input = f.bind_selected("A").await;
    let command = read(&mut input).await;
    assert_eq!(command["cmd"], "resume");
    input_status(&mut input, "delivered", &command).await;
    f.frame().await;
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
