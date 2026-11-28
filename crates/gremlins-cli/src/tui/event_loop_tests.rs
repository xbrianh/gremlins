use super::*;
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use serde_json::{json, Value};
use tokio::net::UnixStream;

const WAIT: Duration = Duration::from_secs(5);

struct Sandbox {
    _lock: std::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl Sandbox {
    fn new() -> Self {
        let lock = test_support::CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::Builder::new()
            .prefix("loop-")
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

struct Harness {
    _sandbox: Sandbox,
    keys: mpsc::UnboundedSender<Event>,
    snapshots: mpsc::UnboundedReceiver<LoopSnapshot>,
    latest: Option<LoopSnapshot>,
    rows: Value,
    watch_selection_override: mpsc::UnboundedSender<String>,
    barrier: u16,
    control: BufReader<UnixStream>,
    session: BufReader<UnixStream>,
    aux: Vec<BufReader<UnixStream>>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

async fn read(peer: &mut BufReader<UnixStream>) -> Value {
    tokio::time::timeout(WAIT, socket::read_json_line(peer))
        .await
        .expect("socket request timed out")
        .unwrap()
        .expect("socket closed")
}

async fn reply(peer: &mut BufReader<UnixStream>, value: Value) {
    socket::write_json_line(peer.get_mut(), &value)
        .await
        .unwrap();
}

fn run(id: &str) -> Target {
    Target::Run { run_id: id.into() }
}

impl Harness {
    async fn start(rows: Value, auxiliary: usize) -> Self {
        let sandbox = Sandbox::new();
        let mut streams = Vec::new();
        let mut aux = Vec::new();
        for _ in 0..auxiliary {
            let (stream, peer) = UnixStream::pair().unwrap();
            streams.push(stream);
            aux.push(BufReader::new(peer));
        }
        let (client, [control, session], event_rx, chat_rx, raw_rx) =
            client::DaemonClient::fake_with_aux_streams_and_channels(
                json!({"session_id":"test", "chat_run_id":"session_chat"}),
                streams,
            );
        let (keys, ct_rx) = mpsc::unbounded_channel();
        let (observe, snapshots) = mpsc::unbounded_channel();
        let (watch_selection_override, selection_rx) = mpsc::unbounded_channel();
        let io = LoopIo {
            session: crate::daemon_client::DaemonSession::from_client(
                client, event_rx, chat_rx, raw_rx,
            ),
            ct_rx,
            suspend_handle: SuspendHandle::new().0,
            observe: Some(observe),
            watch_selection_override: selection_rx,
        };
        let task = tokio::spawn(async move {
            let mut live = LiveArea::with_backend(TestBackend::new(100, 35), 100, 35);
            run_app_with_io(&mut live, io).await
        });
        let mut h = Self {
            _sandbox: sandbox,
            keys,
            snapshots,
            latest: None,
            rows: rows.clone(),
            watch_selection_override,
            barrier: 100,
            control: BufReader::new(control),
            session: BufReader::new(session),
            aux,
            task,
        };
        assert_eq!(read(&mut h.control).await, json!({"op":"ls"}));
        reply(&mut h.control, json!({"gremlins":rows})).await;
        assert_eq!(read(&mut h.session).await["op"], "chat_model");
        reply(&mut h.session, json!({"model":"test-model"})).await;
        h.until(|s| s.screen == Screen::Chat).await;
        h
    }

    async fn until(&mut self, predicate: impl Fn(&LoopSnapshot) -> bool) -> LoopSnapshot {
        if let Some(s) = &self.latest {
            if predicate(s) {
                return s.clone();
            }
        }
        tokio::time::timeout(WAIT, async {
            loop {
                let s = self.snapshots.recv().await.expect("event loop stopped");
                self.latest = Some(s.clone());
                if predicate(&s) {
                    return s;
                }
            }
        })
        .await
        .expect("snapshot predicate timed out")
    }

    fn send_key(&self, code: KeyCode, modifiers: KeyModifiers) {
        self.keys
            .send(Event::Key(KeyEvent::new(code, modifiers)))
            .unwrap();
    }

    async fn key(&mut self, code: KeyCode) -> LoopSnapshot {
        self.chord(code, KeyModifiers::NONE).await
    }

    async fn chord(&mut self, code: KeyCode, modifiers: KeyModifiers) -> LoopSnapshot {
        while self.snapshots.try_recv().is_ok() {}
        self.latest = None;
        self.send_key(code, modifiers);
        self.barrier += 1;
        let width = self.barrier;
        self.keys.send(Event::Resize(width, 35)).unwrap();
        self.until(|s| s.width == width).await
    }

    async fn text(&mut self, text: &str) -> LoopSnapshot {
        self.latest = None;
        for ch in text.chars() {
            self.send_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        self.until(|s| s.input == text).await
    }

    async fn watch(&mut self) -> LoopSnapshot {
        self.latest = None;
        self.send_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let s = self.until(|s| s.screen == Screen::Runs).await;
        // Every watch entry performs the real status refresh.
        assert_eq!(read(&mut self.control).await["op"], "ls");
        reply(&mut self.control, json!({"gremlins":self.rows})).await;
        s
    }

    async fn highlight(&mut self, id: &str) -> LoopSnapshot {
        let mut s = self.key(KeyCode::Null).await;
        for _ in 0..20 {
            if s.watch_selection.as_deref() == Some(id) {
                return s;
            }
            let current = s
                .watch_ids
                .iter()
                .position(|row| Some(row) == s.watch_selection.as_ref())
                .unwrap();
            let desired = s
                .watch_ids
                .iter()
                .position(|row| row == id)
                .expect("missing watch row");
            s = self
                .key(if current < desired {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                })
                .await;
        }
        panic!("row not reachable: {id}");
    }

    async fn assert_quiet(&mut self) {
        for peer in &mut self.aux {
            match tokio::time::timeout(Duration::from_millis(30), socket::read_json_line(peer))
                .await
            {
                Err(_) | Ok(Ok(None)) => {}
                other => panic!("passive navigation sent an auxiliary command: {other:?}"),
            }
        }
        for peer in [&mut self.control, &mut self.session] {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), socket::read_json_line(peer))
                    .await
                    .is_err(),
                "navigation sent a request on a primary socket"
            );
        }
    }

    async fn stop(mut self) {
        let s = self.key(KeyCode::End).await;
        for _ in s.input.chars() {
            self.send_key(KeyCode::Backspace, KeyModifiers::NONE);
        }
        self.key(KeyCode::End).await;
        self.send_key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn daemon_session_close_is_idempotent() {
    let (client, [control, session], event_rx, chat_rx, raw_rx) =
        client::DaemonClient::fake_with_aux_streams_and_channels(
            json!({"session_id":"test", "chat_run_id":"session_chat"}),
            Vec::new(),
        );
    let mut daemon =
        crate::daemon_client::DaemonSession::from_client(client, event_rx, chat_rx, raw_rx);
    let client = Arc::clone(daemon.client());
    daemon.close().await;
    assert!(client.chat_sender().is_closed());
    for take_after_close in [
        |daemon: &mut crate::daemon_client::DaemonSession| {
            drop(daemon.take_event_rx());
        },
        |daemon: &mut crate::daemon_client::DaemonSession| {
            drop(daemon.take_chat_rx());
        },
        |daemon: &mut crate::daemon_client::DaemonSession| {
            drop(daemon.take_raw_rx());
        },
    ] {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            take_after_close(&mut daemon);
        }))
        .is_err());
    }
    let generation = client.session_generation().await;
    tokio::time::timeout(WAIT, daemon.close())
        .await
        .expect("second close blocked");
    assert_eq!(client.session_generation().await, generation);
    drop(client);
    drop(daemon);
    for peer in [control, session] {
        assert_eq!(
            tokio::time::timeout(WAIT, socket::read_json_line(&mut BufReader::new(peer)))
                .await
                .expect("daemon socket did not close")
                .unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn ordinary_fake_drop_closes_socket_write_halves() {
    let (client, peers) = client::DaemonClient::fake_with_identity(
        json!({"session_id":"test", "chat_run_id":"session_chat"}),
    );
    drop(client);
    for peer in peers {
        assert_eq!(
            tokio::time::timeout(WAIT, socket::read_json_line(&mut BufReader::new(peer)))
                .await
                .expect("ordinary fake retained its socket write half")
                .unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn resize_error_closes_control_and_session_sockets() {
    let _sandbox = Sandbox::new();
    let (client, [control, session], event_rx, chat_rx, raw_rx) =
        client::DaemonClient::fake_with_reader_lifetime(
            json!({"session_id":"test", "chat_run_id":"session_chat"}),
            Vec::new(),
            true,
        );
    let (keys, ct_rx) = mpsc::unbounded_channel();
    let (observe, mut snapshots) = mpsc::unbounded_channel();
    let (_selection_tx, selection_rx) = mpsc::unbounded_channel();
    let io = LoopIo {
        session: crate::daemon_client::DaemonSession::from_client(
            client, event_rx, chat_rx, raw_rx,
        ),
        ct_rx,
        suspend_handle: SuspendHandle::new().0,
        observe: Some(observe),
        watch_selection_override: selection_rx,
    };
    let task = tokio::spawn(async move {
        let mut live = LiveArea::with_backend(TestBackend::new(100, 35), 100, 35);
        run_app_loop(&mut live, io, |_, _, _| {
            Err(io::Error::other("injected resize failure"))
        })
        .await
    });
    let mut control = BufReader::new(control);
    let mut session = BufReader::new(session);
    assert_eq!(read(&mut control).await, json!({"op":"ls"}));
    reply(&mut control, json!({"gremlins":[]})).await;
    assert_eq!(read(&mut session).await["op"], "chat_model");
    reply(&mut session, json!({"model":"test-model"})).await;
    tokio::time::timeout(WAIT, async {
        loop {
            if snapshots.recv().await.expect("event loop stopped").screen == Screen::Chat {
                break;
            }
        }
    })
    .await
    .expect("chat startup timed out");

    keys.send(Event::Resize(101, 35)).unwrap();
    let error = tokio::time::timeout(WAIT, task)
        .await
        .expect("event loop did not exit after resize failure")
        .expect("event loop task panicked")
        .expect_err("resize failure should propagate");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), "injected resize failure");
    // This fake opts into retaining write halves: dropping LoopIo cannot produce EOF.
    let (control_eof, session_eof) = tokio::join!(
        tokio::time::timeout(WAIT, socket::read_json_line(&mut control)),
        tokio::time::timeout(WAIT, socket::read_json_line(&mut session)),
    );
    for (name, eof) in [("control", control_eof), ("session", session_eof)] {
        assert_eq!(
            eof.unwrap_or_else(|_| panic!("{name} socket stayed open after resize failure"))
                .unwrap_or_else(|error| panic!("failed to read {name} socket: {error}")),
            None,
            "{name} socket should reach EOF after resize failure"
        );
    }
}

#[tokio::test]
async fn enqueue_identity_installs_before_turn_started_without_stealing_b() {
    let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 3).await;
    h.text("first reserved prompt").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.active_request).await;
    assert_eq!(
        read(&mut h.aux[0]).await,
        json!({"op":"watch","scope":"run","id":"session_chat","session_id":"test","replay":false})
    );
    reply(&mut h.aux[0], json!({"type":"watched","scope":"run","id":"session_chat","stage":null,"stage_run_id":null,"replay":false})).await;
    let request = read(&mut h.session).await;
    assert_eq!(request["op"], "chat");
    assert_eq!(request["text"], "first reserved prompt");
    assert_eq!(request["session_id"], "test");
    assert!(request.get("target").is_none());
    h.watch().await;
    h.highlight("B").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.screen == Screen::Chat && s.selected_target == run("B"))
        .await;
    assert_eq!(read(&mut h.aux[1]).await["id"], "B");
    reply(&mut h.aux[1], json!({"type":"watched","scope":"run","id":"B","stage":null,"stage_run_id":null,"replay":true})).await;
    reply(
        &mut h.aux[1],
        json!({"type":"replay_checkpoint","id":"B","complete":true}),
    )
    .await;
    h.text("B draft before enqueue ack").await;
    reply(
        &mut h.session,
        json!({"type":"ok","chat_run_id":"replacement","enqueued":true,"session_id":"test"}),
    )
    .await;
    let s = h
        .until(|s| s.chat_run_id.as_deref() == Some("replacement"))
        .await;
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft before enqueue ack");
    assert_eq!(s.screen, Screen::Chat);
    // No TurnStarted has been sent on any channel: the submitted owner survives.
    let turn = request["turn"].as_str().expect("submitted turn");
    assert_eq!(s.session_turn.as_deref(), Some(turn));
    assert!(s.session_execution.is_none());
    assert!(s.session_has_turn);
    assert_eq!(
        read(&mut h.aux[2]).await,
        json!({"op":"watch","scope":"run","id":"replacement","session_id":"test","replay":true})
    );
    reply(&mut h.aux[2], json!({"type":"watched","scope":"run","id":"replacement","stage":null,"stage_run_id":null,"replay":true})).await;
    reply(
        &mut h.aux[2],
        json!({"type":"replay_checkpoint","id":"replacement","complete":true}),
    )
    .await;
    reply(&mut h.aux[2], json!({"type":"turn_started","run_id":"replacement","turn":turn,"stage_type":"agent","stage_run_id":"replacement-execution"})).await;
    let s = h.until(|s| s.session_execution.is_some()).await;
    assert_eq!(s.session_turn.as_deref(), Some(turn));
    let execution = s.session_execution.unwrap();
    assert_eq!(execution.run_id, "replacement");
    assert_eq!(execution.stage_run_id, "replacement-execution");
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft before enqueue ack");
    let output = "authoritative replacement answer";
    reply(&mut h.aux[2], json!({"type":"stream_chunk","run_id":"replacement","turn":turn,"stage_type":"agent","stage_run_id":"replacement-execution","text":output})).await;
    let s = h.until(|s| s.session_response == output).await;
    assert_eq!(s.session_turn.as_deref(), Some(turn));
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft before enqueue ack");
    reply(&mut h.aux[2], json!({"type":"done","run_id":"replacement","turn":turn,"stage_type":"agent","stage_run_id":"replacement-execution","text":output,"summary":""})).await;
    let s = h
        .until(|s| s.session_turn.is_none() && !s.session_has_turn)
        .await;
    assert!(s.session_execution.is_none());
    assert_eq!(s.session_transcript.matches(output).count(), 1);
    assert!(!s.old_session_transcript.contains(output));
    assert!(!s.selected_transcript.contains(output));
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft before enqueue ack");
    assert_eq!(s.screen, Screen::Chat);
    h.stop().await;
}

#[tokio::test]
async fn reset_aliases_on_b_are_rejected_without_clearing_the_composer() {
    for alias in ["/clear", "/new"] {
        let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 1).await;
        h.watch().await;
        h.highlight("B").await;
        h.send_key(KeyCode::Enter, KeyModifiers::NONE);
        h.until(|s| s.screen == Screen::Chat && s.selected_target == run("B"))
            .await;
        assert_eq!(read(&mut h.aux[0]).await["op"], "watch");
        reply(
            &mut h.aux[0],
            json!({"type":"error","message":"watch rejected"}),
        )
        .await;
        h.text(alias).await;
        let s = h.key(KeyCode::Enter).await;
        assert_eq!(s.selected_target, run("B"));
        assert_eq!(s.input, alias, "rejected reset must preserve B's composer");
        assert!(!s.session_recreating);
        assert_eq!(
            s.reset_rejection,
            Some("select the session chat in Watch before resetting it")
        );
        assert_eq!(s.chat_run_id.as_deref(), Some("session_chat"));
        h.assert_quiet().await;
        h.stop().await;
    }
}

#[tokio::test]
async fn late_successful_reset_reply_installs_reservation_without_touching_b() {
    let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 1).await;
    let listener =
        socket::bind_session_socket(&gremlins::config::state_root(), "replacement-session")
            .unwrap();
    h.text("/new").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.session_recreating).await;
    assert_eq!(read(&mut h.session).await["op"], "close_session");
    h.watch().await;
    h.highlight("B").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.screen == Screen::Chat && s.selected_target == run("B"))
        .await;
    assert_eq!(read(&mut h.aux[0]).await["id"], "B");
    reply(&mut h.aux[0], json!({"type":"watched","scope":"run","id":"B","stage":null,"stage_run_id":null,"replay":true})).await;
    reply(
        &mut h.aux[0],
        json!({"type":"replay_checkpoint","id":"B","complete":true}),
    )
    .await;
    h.text("B draft survives success").await;
    reply(&mut h.session, json!({"type":"ok"})).await;
    assert_eq!(read(&mut h.control).await["op"], "create_session");
    reply(&mut h.control, json!({"type":"ok","session_id":"replacement-session","chat_run_id":"replacement-reserved"})).await;
    let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut replacement = BufReader::new(stream);
    let s = h
        .until(|s| {
            !s.session_recreating && s.chat_run_id.as_deref() == Some("replacement-reserved")
        })
        .await;
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft survives success");
    assert_eq!(s.screen, Screen::Chat);
    assert_eq!(read(&mut replacement).await["op"], "chat_model");
    reply(&mut replacement, json!({"model":"replacement-model"})).await;
    let s = h.watch().await;
    assert_eq!(s.screen, Screen::Runs);
    assert!(s.watch_ids.contains(&"replacement-reserved".to_owned()));
    assert!(!s.watch_ids.contains(&"session_chat".to_owned()));
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft survives success");
    let s = h.key(KeyCode::Esc).await;
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft survives success");
    h.stop().await;
}

#[tokio::test]
async fn late_failed_reset_reply_does_not_touch_b_draft() {
    let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 1).await;
    h.text("/clear").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.session_recreating).await;
    assert_eq!(read(&mut h.session).await["op"], "close_session");
    // Leave the reset request pending while selecting another conversation.
    h.watch().await;
    h.highlight("B").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.screen == Screen::Chat && s.selected_target == run("B"))
        .await;
    assert_eq!(read(&mut h.aux[0]).await["op"], "watch");
    reply(
        &mut h.aux[0],
        json!({"type":"error","message":"watch rejected"}),
    )
    .await;
    h.text("B draft survives reset").await;
    reply(&mut h.session, json!({"type":"ok"})).await;
    assert_eq!(read(&mut h.control).await["op"], "create_session");
    reply(
        &mut h.control,
        json!({"type":"error","message":"reset failed"}),
    )
    .await;
    let s = h.until(|s| !s.session_recreating).await;
    assert_eq!(s.selected_target, run("B"));
    assert_eq!(s.input, "B draft survives reset");
    assert_eq!(s.screen, Screen::Chat);
    assert_eq!(s.chat_run_id.as_deref(), Some("session_chat"));
    h.assert_quiet().await;
    h.stop().await;
}

#[tokio::test]
async fn failed_inline_follow_and_late_old_socket_do_not_change_selection_or_draft() {
    let mut h = Harness::start(
        json!([
            {"id":"A","status":"running"}, {"id":"B","status":"running"}
        ]),
        2,
    )
    .await;
    h.text("private draft").await;
    h.watch().await;
    h.highlight("A").await;
    h.chord(KeyCode::Char('o'), KeyModifiers::CONTROL).await;
    assert_eq!(read(&mut h.aux[0]).await["id"], "A");
    // Change rows before the old follow acknowledges or emits output.
    h.highlight("B").await;
    h.chord(KeyCode::Char('o'), KeyModifiers::CONTROL).await;
    assert_eq!(read(&mut h.aux[1]).await["id"], "B");
    let _ = socket::write_json_line(
        h.aux[0].get_mut(),
        &json!({"type":"error","message":"stale A failure"}),
    )
    .await;
    reply(
        &mut h.aux[1],
        json!({"type":"error","message":"B follow failed"}),
    )
    .await;
    assert_eq!(read(&mut h.session).await["op"], "read_blob");
    reply(&mut h.session, json!({"lines":[]})).await;
    let s = h.until(|s| s.watch_follow_error.is_some()).await;
    assert_eq!(s.selected_target, run("session_chat"));
    assert_eq!(s.input, "private draft");
    assert_eq!(s.watch_selection.as_deref(), Some("B"));
    h.key(KeyCode::Esc).await;
    h.key(KeyCode::Esc).await;
    h.assert_quiet().await;
    h.stop().await;
}

#[tokio::test]
async fn watch_highlight_toggle_and_dismiss_preserve_selected_draft() {
    let mut h = Harness::start(
        json!([
            {"id":"A","status":"running"}, {"id":"B","status":"running"}
        ]),
        2,
    )
    .await;
    h.watch().await;
    h.highlight("A").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    h.until(|s| s.screen == Screen::Chat && s.selected_target == run("A"))
        .await;
    assert_eq!(
        read(&mut h.aux[0]).await,
        json!({"op":"watch","scope":"run","id":"A","session_id":"test","replay":true})
    );
    reply(&mut h.aux[0], json!({"type":"watched","scope":"run","id":"A","replay":true,"stage":null,"stage_run_id":null})).await;
    reply(
        &mut h.aux[0],
        json!({"type":"replay_checkpoint","id":"A","complete":true}),
    )
    .await;
    h.text("A private draft").await;
    h.watch().await;
    let s = h.highlight("B").await;
    assert_eq!(s.selected_target, run("A"));
    assert_eq!(s.input, "A private draft");
    h.chord(KeyCode::Char('o'), KeyModifiers::CONTROL).await;
    let request = read(&mut h.aux[1]).await;
    assert_eq!(
        request,
        json!({"op":"watch","scope":"run","id":"B","session_id":"test","replay":true})
    );
    reply(&mut h.aux[1], json!({"type":"watched","scope":"run","id":"B","replay":true,"stage":null,"stage_run_id":null})).await;
    let s = h.until(|s| s.watch_following.as_deref() == Some("B")).await;
    assert_eq!(s.selected_target, run("A"));
    assert_eq!(s.input, "A private draft");
    h.chord(KeyCode::Char('w'), KeyModifiers::CONTROL).await;
    let s = h.until(|s| s.watch_following.is_none()).await;
    assert_eq!(s.selected_target, run("A"));
    assert_eq!(s.input, "A private draft");
    h.key(KeyCode::Esc).await;
    let s = h.until(|s| s.screen == Screen::Chat).await;
    assert_eq!(s.selected_target, run("A"));
    assert_eq!(s.input, "A private draft");
    h.assert_quiet().await;
    h.stop().await;
}

#[tokio::test]
async fn repeated_enter_on_b_is_passive_and_restores_its_draft() {
    let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 2).await;
    h.text("session draft").await;
    for i in 0..2 {
        h.watch().await;
        h.highlight("B").await;
        h.send_key(KeyCode::Enter, KeyModifiers::NONE);
        let s = h
            .until(|s| s.screen == Screen::Chat && s.selected_target == run("B"))
            .await;
        assert_eq!(s.input, if i == 0 { "" } else { "B draft" });
        let request = read(&mut h.aux[i]).await;
        assert_eq!(
            request,
            json!({"op":"watch","scope":"run","id":"B","session_id":"test","replay":true})
        );
        reply(&mut h.aux[i], json!({"type":"watched","scope":"run","id":"B","replay":true,"stage":null,"stage_run_id":null})).await;
        reply(
            &mut h.aux[i],
            json!({"type":"replay_checkpoint","id":"B","complete":true}),
        )
        .await;
        if i == 0 {
            h.text("B draft").await;
        }
        h.assert_quiet().await;
    }
    h.watch().await;
    h.highlight("session_chat").await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    let s = h
        .until(|s| s.screen == Screen::Chat && s.selected_target == run("session_chat"))
        .await;
    assert_eq!(s.input, "session draft");
    h.assert_quiet().await;
    h.stop().await;
}

#[tokio::test]
async fn empty_discovery_watch_toggle_failure_and_dismiss_keep_reserved_draft() {
    let mut h = Harness::start(json!([]), 1).await;
    h.text("empty discovery draft").await;
    let s = h.watch().await;
    assert_eq!(s.watch_ids, vec!["session_chat"]);
    h.chord(KeyCode::Char('o'), KeyModifiers::CONTROL).await;
    assert_eq!(
        read(&mut h.aux[0]).await,
        json!({"op":"watch","scope":"run","id":"session_chat","session_id":"test","replay":true})
    );
    reply(
        &mut h.aux[0],
        json!({"type":"error","message":"reservation not started"}),
    )
    .await;
    assert_eq!(read(&mut h.session).await["op"], "read_blob");
    reply(&mut h.session, json!({"lines":[]})).await;
    let s = h.until(|s| s.watch_follow_error.is_some()).await;
    assert_eq!(s.input, "empty discovery draft");
    assert_eq!(s.selected_target, run("session_chat"));
    h.key(KeyCode::Esc).await; // Close the failed pane first.
    let s = h.key(KeyCode::Esc).await;
    assert_eq!(s.screen, Screen::Chat);
    assert_eq!(s.input, "empty discovery draft");
    h.assert_quiet().await;
    h.stop().await;
}

#[tokio::test]
async fn removed_highlight_enter_keeps_reserved_draft_and_sends_no_input() {
    let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 0).await;
    h.text("reserved draft").await;
    h.watch().await;
    h.highlight("B").await;
    reply(
        &mut h.control,
        serde_json::to_value(gremlins_magic_bus::EventKind::RunRemoved { id: "B".into() }).unwrap(),
    )
    .await;
    h.until(|s| s.watch_ids == vec!["session_chat"]).await;
    // Removal normally falls back to the reservation. Recreate a stale highlight
    // so Enter must exercise the unresolved-row branch in the real loop.
    h.watch_selection_override.send("B".into()).unwrap();
    let before = h.key(KeyCode::Null).await;
    assert_eq!(before.watch_selection.as_deref(), Some("B"));
    assert!(!before.watch_ids.contains(&"B".to_owned()));
    let s = h.key(KeyCode::Enter).await;
    assert_eq!(s.screen, Screen::Runs);
    assert_eq!(s.selected_target, before.selected_target);
    assert_eq!(s.attachment_generation, before.attachment_generation);
    assert_eq!(s.attachment_target, before.attachment_target);
    assert_eq!(s.input, "reserved draft");
    h.assert_quiet().await;
    h.key(KeyCode::Esc).await;
    h.stop().await;
}

#[tokio::test]
async fn fresh_reservation_survives_leave_and_return_before_first_prompt() {
    let mut h = Harness::start(json!([{"id":"B","status":"running"}]), 2).await;
    h.text("reserved first prompt").await;
    h.watch().await;
    h.highlight("B").await;
    h.key(KeyCode::Enter).await;
    assert_eq!(read(&mut h.aux[0]).await["id"], "B");
    reply(&mut h.aux[0], json!({"type":"watched","scope":"run","id":"B","stage":null,"stage_run_id":null,"replay":true})).await;
    reply(
        &mut h.aux[0],
        json!({"type":"replay_checkpoint","id":"B","complete":true}),
    )
    .await;
    h.watch().await;
    h.highlight("session_chat").await;
    let s = h.key(KeyCode::Enter).await;
    assert_eq!(s.selected_target, run("session_chat"));
    assert_eq!(s.input, "reserved first prompt");
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        read(&mut h.aux[1]).await,
        json!({"op":"watch","scope":"run","id":"session_chat","session_id":"test","replay":false})
    );
    reply(&mut h.aux[1], json!({"type":"watched","scope":"run","id":"session_chat","stage":null,"stage_run_id":null,"replay":false})).await;
    let request = read(&mut h.session).await;
    assert_eq!(request["op"], "chat");
    assert_eq!(request["text"], "reserved first prompt");
    assert_eq!(request["session_id"], "test");
    assert!(request.get("target").is_none());
    reply(
        &mut h.session,
        json!({"type":"ok","chat_run_id":"session_chat","enqueued":true}),
    )
    .await;
    h.stop().await;
}

#[tokio::test]
async fn reserved_session_row_returns_to_prompt_and_discovery_is_deduplicated() {
    let mut h = Harness::start(json!([]), 1).await;
    h.text("reserved draft").await;
    let s = h.watch().await;
    assert_eq!(s.watch_ids, vec!["session_chat"]);
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    let s = h.until(|s| s.screen == Screen::Chat).await;
    assert_eq!(s.selected_target, run("session_chat"));
    assert_eq!(s.input, "reserved draft");
    let boundary = json!({"type":"stage_boundary","run_id":"session_chat","stage":"chat","stage_run_id":"child","stage_type":"agent","active":true});
    reply(&mut h.control, boundary.clone()).await;
    reply(&mut h.control, boundary).await;
    h.watch().await;
    let s = h.until(|s| s.watch_ids.len() == 2).await;
    assert_eq!(
        s.watch_ids
            .iter()
            .filter(|id| id.as_str() == "session_chat")
            .count(),
        1
    );
    let child = s
        .watch_ids
        .iter()
        .find(|id| id.as_str() != "session_chat")
        .unwrap()
        .clone();
    h.highlight(&child).await;
    h.send_key(KeyCode::Enter, KeyModifiers::NONE);
    let target = Target::Invocation {
        run_id: "session_chat".into(),
        stage: "chat".into(),
        stage_run_id: "child".into(),
    };
    let s = h
        .until(|s| s.screen == Screen::Chat && s.selected_target == target)
        .await;
    assert!(s.input.is_empty());
    assert_eq!(
        read(&mut h.aux[0]).await,
        json!({"op":"watch","scope":"run","id":"session_chat","session_id":"test","replay":true,"stage":"chat","stage_run_id":"child"})
    );
    h.assert_quiet().await;
    h.stop().await;
}
