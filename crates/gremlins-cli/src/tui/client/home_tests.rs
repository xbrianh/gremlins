use super::*;
use tokio::net::UnixStream;

#[tokio::test]
async fn stale_prepare_cannot_retire_successor_display() {
    let (stream, peer) = UnixStream::pair().unwrap();
    let (client, [_control, _session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![stream],
    );
    let successor = tokio::spawn(std::future::pending::<()>());
    let successor_abort = successor.abort_handle();
    let server = async {
        let mut observer = watch(peer, true).await;
        {
            let mut display = client.home_display.lock().await;
            client.session.write.lock().await.generation = 1;
            *display = Some(Display { forward: successor });
        }
        assert!(socket::read_json_line(&mut observer)
            .await
            .unwrap()
            .is_none());
    };
    let send = client.send_session_request_for_generation(json!({"op":"chat", "turn":"old"}), 0);
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(send, server)
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().contains("retired session"));
    assert!(client.home_display.lock().await.is_some());
    assert!(!successor_abort.is_finished());
    client.shutdown().await;
}

#[tokio::test]
async fn live_only_home_watch_rejects_replay_ack_and_closes_reader() {
    let (stream, peer) = UnixStream::pair().unwrap();
    let (client, _peers) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![stream],
    );
    let server = async {
        let mut peer = BufReader::new(peer);
        let request = socket::read_json_line(&mut peer).await.unwrap().unwrap();
        assert_eq!(request["replay"], false);
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"watched", "scope":"run",
            "id":"home", "stage":null, "stage_run_id":null, "replay":true}),
        )
        .await
        .unwrap();
        assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(client.prepare_home("home", false), server)
    })
    .await
    .unwrap();
    assert!(result.is_err());
    client.shutdown().await;
}

#[tokio::test]
async fn malformed_home_ack_closes_prepared_reader() {
    for ack in [
        json!({"type":"unexpected"}),
        json!({"type":"ok"}),
        json!({"type":"ok", "id":"home", "session_id":"retired"}),
    ] {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![stream],
        );
        let server = async {
            let mut observer = watch(peer, true).await;
            let mut session = BufReader::new(session);
            socket::read_json_line(&mut session).await.unwrap().unwrap();
            socket::write_json_line(session.get_mut(), &ack)
                .await
                .unwrap();
            assert!(socket::read_json_line(&mut observer)
                .await
                .unwrap()
                .is_none());
        };
        let send =
            client.send_session_request_for_generation(json!({"op":"chat", "turn":"turn"}), 0);
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(send, server)
        })
        .await
        .unwrap();
        assert!(result.is_err());
        client.shutdown().await;
    }
}

#[tokio::test]
async fn home_watch_failure_and_eof_recover_by_replay_without_resubmission() {
    for (reject, exhaust_initial_watches) in [(true, false), (false, false), (true, true)] {
        let (initial, initial_peer) = UnixStream::pair().unwrap();
        let (recovery, recovery_peer) = UnixStream::pair().unwrap();
        let (failed, failed_peer) = UnixStream::pair().unwrap();
        let (retry, retry_peer) = UnixStream::pair().unwrap();
        let streams = if exhaust_initial_watches {
            vec![initial, failed, retry, recovery]
        } else if reject {
            vec![initial, failed, recovery]
        } else {
            vec![initial, recovery]
        };
        let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            streams,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        client.out = Arc::new(OutState {
            chat_tx: tx,
            event_tx: mpsc::unbounded_channel().0,
            raw_tx: mpsc::unbounded_channel().0,
        });
        let server = tokio::spawn(async move {
            let observer = watch(initial_peer, !exhaust_initial_watches).await;
            let mut session = BufReader::new(session);
            assert_eq!(
                socket::read_json_line(&mut session).await.unwrap().unwrap()["op"],
                "chat"
            );
            socket::write_json_line(
                session.get_mut(),
                &json!({"type":"ok", "id": if reject { "replacement" } else { "home" }}),
            )
            .await
            .unwrap();
            drop(observer);
            let failed_watches = if exhaust_initial_watches {
                vec![failed_peer, retry_peer]
            } else if reject {
                vec![failed_peer]
            } else {
                Vec::new()
            };
            for peer in failed_watches {
                watch_run(peer, "replacement", true, false).await;
            }
            let run = if reject { "replacement" } else { "home" };
            let mut recovery = watch_run(recovery_peer, run, true, true).await;
            socket::write_json_line(
                recovery.get_mut(),
                &json!({"type":"done", "text":"recovered", "summary":"",
                "run_id":run, "turn":"turn"}),
            )
            .await
            .unwrap();
            // Keep the submission socket open to detect accidental resubmission.
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(50),
                socket::read_json_line(&mut session)
            )
            .await
            .is_err());
        });
        assert_eq!(
            client
                .send_session_request_for_generation(json!({"op":"chat", "turn":"turn"}), 0)
                .await
                .unwrap()["type"],
            "ok"
        );
        let (event, turn, _) = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(event).unwrap()["text"], "recovered");
        assert_eq!(turn.as_deref(), Some("turn"));
        server.await.unwrap();
        client.shutdown().await;
    }
}

#[tokio::test]
async fn home_recovery_retries_failures_and_deduplicates_replayed_sequences() {
    for initial_failure in [false, true] {
        let (tx, rx) = mpsc::channel(4);
        let chunk = json!({"type":"stream_chunk", "text":"once", "run_id":"home",
            "turn":"turn", "seq":42});
        if !initial_failure {
            tx.send(chunk.clone()).await.unwrap();
        }
        drop(tx);
        let (out_tx, mut output) = mpsc::unbounded_channel();
        let (client, _peers) =
            DaemonClient::fake_with_identity(json!({"session_id":"owner", "home_run_id":"home"}));
        let out = Arc::new(OutState {
            chat_tx: out_tx,
            event_tx: mpsc::unbounded_channel().0,
            raw_tx: mpsc::unbounded_channel().0,
        });
        let mut attempts = 0;
        let forward = tokio::spawn(forward_home_stream(
            rx,
            Reader(tokio::spawn(async {})),
            HomeWatchContext {
                session_write: Arc::clone(&client.session.write),
                out,
                generation: 0,
                run: "home".into(),
                turn: Some("turn".into()),
            },
            None,
            move || {
                attempts += 1;
                let chunk = chunk.clone();
                async move {
                    if attempts < 3 {
                        return Err("temporarily unavailable".into());
                    }
                    let (tx, rx) = mpsc::channel(4);
                    tx.send(chunk.clone()).await.unwrap();
                    tx.send(chunk).await.unwrap();
                    tx.send(json!({"type":"done", "text":"finished", "summary":"",
                        "run_id":"home", "turn":"turn", "seq":43}))
                        .await
                        .unwrap();
                    Ok((rx, Reader(tokio::spawn(async {}))))
                }
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), forward)
            .await
            .unwrap()
            .unwrap();
        let mut chunks = 0;
        let mut terminal = 0;
        while let Ok((event, _, _)) = output.try_recv() {
            let event = serde_json::to_value(event).unwrap();
            chunks += usize::from(event["type"] == "stream_chunk");
            terminal += usize::from(event["type"] == "done");
        }
        assert_eq!(chunks, 1);
        assert_eq!(terminal, 1);
        client.shutdown().await;
    }
}

async fn watch(peer: UnixStream, live: bool) -> BufReader<UnixStream> {
    watch_run(peer, "home", false, live).await
}

async fn watch_run(
    peer: UnixStream,
    id: &str,
    replay: bool,
    accept: bool,
) -> BufReader<UnixStream> {
    let mut peer = BufReader::new(peer);
    let req = socket::read_json_line(&mut peer).await.unwrap().unwrap();
    assert_eq!(
        req,
        json!({"op":"watch", "scope":"run", "id":id,
        "session_id":"owner", "replay":replay})
    );
    let ack = if accept {
        json!({"type":"watched", "scope":"run", "id":id, "stage":null,
            "stage_run_id":null, "replay":replay})
    } else {
        json!({"type":"error", "message":"not live"})
    };
    socket::write_json_line(peer.get_mut(), &ack).await.unwrap();
    peer
}

#[tokio::test]
async fn stale_enqueue_ack_cannot_replace_home_identity() {
    let (stream, peer) = UnixStream::pair().unwrap();
    let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![stream],
    );
    let server = tokio::spawn(async move {
        let _observer = watch(peer, true).await;
        let mut session = BufReader::new(session);
        socket::read_json_line(&mut session).await.unwrap().unwrap();
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"ok", "id":"stale-home", "session_id":"retired"}),
        )
        .await
        .unwrap();
    });
    let error = client
        .send_session_request_for_generation(json!({"op":"chat", "text":"hi", "turn":"turn"}), 0)
        .await
        .unwrap_err();
    assert!(error.contains("retired session"));
    assert_eq!(client.home_run_id().await, "home");
    assert!(client
        .send_session_request_for_generation(json!({"op":"chat"}), 1)
        .await
        .is_err());
    server.await.unwrap();
    client.shutdown().await;
}

#[tokio::test]
async fn generation_reset_while_replacement_prepare_home_awaits_rejects_display() {
    generation_reset_during_home_enqueue(true).await;
}

#[tokio::test]
async fn generation_reset_before_enqueue_ack_preserves_new_home_identity() {
    generation_reset_during_home_enqueue(false).await;
}

async fn generation_reset_during_home_enqueue(during_prepare: bool) {
    let (initial, initial_peer) = UnixStream::pair().unwrap();
    let (replacement, replacement_peer) = UnixStream::pair().unwrap();
    let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![initial, replacement],
    );
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    client.out = Arc::new(OutState {
        chat_tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx,
    });
    let (waiting_tx, waiting_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = async move {
        let _initial = watch(initial_peer, true).await;
        let mut session = BufReader::new(session);
        let request = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(request["op"], "chat");
        assert_eq!(request["session_id"], "owner");
        assert_eq!(request["turn"], "old-turn");
        if !during_prepare {
            waiting_tx.send(()).unwrap();
            release_rx.await.unwrap();
            socket::write_json_line(
                session.get_mut(),
                &json!({"type":"ok", "id":"replacement", "session_id":"owner"}),
            )
            .await
            .unwrap();
            return;
        }
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"ok", "id":"replacement", "session_id":"owner"}),
        )
        .await
        .unwrap();
        let mut replacement = BufReader::new(replacement_peer);
        let request = socket::read_json_line(&mut replacement)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request["op"], "watch");
        assert_eq!(request["id"], "replacement");
        assert_eq!(request["session_id"], "owner");
        assert_eq!(request["replay"], true);
        waiting_tx.send(()).unwrap();
        release_rx.await.unwrap();
        // Buffer ack and output together so cancellation cannot race the writes.
        let frames = [
            json!({"type":"watched", "scope":"run", "id":"replacement",
                "stage":null, "stage_run_id":null, "replay":true}),
            json!({"type":"stream_chunk", "text":"retired output",
                "run_id":"replacement", "turn":"old-turn"}),
            json!({"type":"done", "text":"retired output", "summary":"",
                "run_id":"replacement", "turn":"old-turn"}),
        ];
        let wire: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
        tokio::io::AsyncWriteExt::write_all(replacement.get_mut(), wire.as_bytes())
            .await
            .unwrap();
    };
    let reset = async {
        waiting_rx.await.unwrap();
        if during_prepare {
            assert_eq!(client.home_run_id().await, "replacement");
            assert_eq!(
                raw_rx.recv().await.unwrap(),
                json!({"type":"home_run_identity", "id":"replacement",
                    "turn":"old-turn", "session_id":"owner", "generation":0})
            );
        }
        assert!(client.home_display.lock().await.is_none());
        {
            // Match the atomic identity publication performed by session reset.
            let mut write = client.session.write.lock().await;
            write.generation = 1;
            write.session_id = Some("new-owner".into());
            write.home_run_id = Some("new-home".into());
        }
        release_tx.send(()).unwrap();
    };
    let send = client.send_session_request_for_generation(
        json!({"op":"chat", "text":"hi", "turn":"old-turn"}),
        0,
    );
    let (result, (), ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(send, reset, server)
    })
    .await
    .unwrap();
    let error = result.unwrap_err();
    assert_eq!(
        error,
        if during_prepare {
            "home display belongs to a retired session"
        } else {
            "home acknowledgment belongs to a retired session"
        }
    );
    assert!(client.home_display.lock().await.is_none());
    {
        let write = client.session.write.lock().await;
        assert_eq!(write.generation, 1);
        assert_eq!(write.session_id.as_deref(), Some("new-owner"));
        assert_eq!(write.home_run_id.as_deref(), Some("new-home"));
    }
    assert!(chat_rx.try_recv().is_err(), "retired output was forwarded");
    assert!(raw_rx.try_recv().is_err(), "retired identity was published");
    client.shutdown().await;
}

async fn exercise(first: bool) {
    let (a, a_peer) = UnixStream::pair().unwrap();
    let (b, b_peer) = UnixStream::pair().unwrap();
    let streams = if first { vec![a, b] } else { vec![a] };
    let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        streams,
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut client = client;
    client.out = Arc::new(OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx: mpsc::unbounded_channel().0,
    });
    let server = tokio::spawn(async move {
        let mut observer = watch(a_peer, !first).await;
        let mut session = BufReader::new(session);
        let req = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(req["op"], "chat");
        assert_eq!(req["turn"], "new-turn");
        // Output can precede the enqueue ack.
        if first {
            socket::write_json_line(
                session.get_mut(),
                &json!({"type":"ok", "id":"home", "status":"streaming"}),
            )
            .await
            .unwrap();
            observer = watch_run(b_peer, "home", true, true).await;
            socket::write_json_line(observer.get_mut(), &json!({"type":"history_gap"}))
                .await
                .unwrap();
        }
        for frame in [
            json!({"type":"stream_chunk", "text":"stale preack output", "run_id":"home", "turn":"new-turn", "correlation_turn":"old-turn"}),
            json!({"type":"turn_started", "stage_type":"agent", "stage_run_id":"execution", "run_id":"home", "turn":"old-turn", "correlation_turn":"new-turn"}),
            json!({"type":"stream_chunk", "text":"child", "run_id":"home/child", "turn":"new-turn"}),
            json!({"type":"stream_chunk", "text":"answer", "run_id":"home", "turn":"new-turn"}),
            json!({"type":"done", "text":"answer", "summary":"", "run_id":"home", "turn":"new-turn"}),
        ] {
            socket::write_json_line(observer.get_mut(), &frame)
                .await
                .unwrap();
        }
        if !first {
            socket::write_json_line(
                session.get_mut(),
                &json!({"type":"ok", "id":"home", "status":"streaming"}),
            )
            .await
            .unwrap();
        }
    });
    let response = client
        .send_session_request_for_generation(
            json!({"op":"chat", "text":"hi", "turn":"new-turn"}),
            0,
        )
        .await
        .unwrap();
    assert_eq!(response["type"], "ok");
    if first {
        let (event, turn, execution) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(event, EventKind::SystemWarning { .. }));
        assert_eq!(turn.as_deref(), Some("new-turn"));
        assert_eq!(execution, None);
    }
    for expected in ["turn_started", "stream_chunk", "done"] {
        let (event, turn, execution) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        let event = serde_json::to_value(event).unwrap();
        assert_eq!(event["type"], expected);
        if expected == "stream_chunk" || expected == "done" {
            assert_eq!(event["text"], "answer");
        }
        assert_eq!(turn.as_deref(), Some("new-turn"));
        assert_eq!(
            execution.map(|e| (e.stage_run_id, e.nonce)),
            (expected == "turn_started").then(|| ("execution".into(), 0))
        );
    }
    server.await.unwrap();
    client.shutdown().await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn home_live_only_watch_accepts_early_output_without_replay_checkpoint() {
    exercise(false).await;
}

#[tokio::test]
async fn first_watch_failure_recovers_with_replay_and_genuine_gap() {
    exercise(true).await;
}

#[test]
fn forward_home_preserves_agent_execution_and_wire_turn_without_start() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx: mpsc::unbounded_channel().0,
    };
    let frame = json!({"type":"stream_chunk", "text":"answer",
        "run_id":"home", "stage_type":"agent", "correlation_stage":"chat", "stage_run_id":"exec",
        "seq":42, "turn":"old-turn", "correlation_turn":"correlated-turn"});
    forward_home(&out, frame.clone(), "home", Some("submitted-turn"));
    let (event, turn, execution) = rx.try_recv().unwrap();
    assert_eq!(serde_json::to_value(event).unwrap()["type"], "stream_chunk");
    assert_eq!(turn.as_deref(), Some("correlated-turn"));
    assert_eq!(
        execution,
        Some(Execution {
            run_id: "home".into(),
            stage_run_id: "exec".into(),
            nonce: 0,
        })
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn home_enqueue_replacement_watch_failure_retains_ack_and_warns() {
    exercise_transport_warning(true).await;
}

#[tokio::test]
async fn home_display_eof_warns_instead_of_error() {
    exercise_transport_warning(false).await;
}

async fn exercise_transport_warning(reject_watch: bool) {
    let (initial, initial_peer) = UnixStream::pair().unwrap();
    let (replacement, replacement_peer) = UnixStream::pair().unwrap();
    let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![initial, replacement],
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    client.out = Arc::new(OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx,
    });
    let ack = json!({"type":"ok", "id":"replacement", "status":"streaming", "session_id":"owner"});
    let expected_ack = ack.clone();
    let server = tokio::spawn(async move {
        let _initial = watch(initial_peer, true).await;
        let mut session = BufReader::new(session);
        let request = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(request["op"], "chat");
        assert_eq!(request["turn"], "new-turn");
        socket::write_json_line(session.get_mut(), &ack)
            .await
            .unwrap();
        watch_run(replacement_peer, "replacement", true, !reject_watch).await;
        // A successful watch then closes without a terminal turn frame.
    });
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_session_request_for_generation(
            json!({"op":"chat", "text":"hi", "turn":"new-turn"}),
            0,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response, expected_ack);
    assert_eq!(client.home_run_id().await, "replacement");
    assert_eq!(
        raw_rx.try_recv().unwrap(),
        json!({"type":"home_run_identity", "id":"replacement",
            "turn":"new-turn", "session_id":"owner", "generation":0})
    );
    let (event, turn, execution) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
    match event {
        EventKind::SystemWarning { turn, message } => {
            assert_eq!(turn, None);
            if reject_watch {
                assert!(
                    message.starts_with("home display unavailable:"),
                    "{message}"
                );
            } else {
                assert_eq!(
                    message,
                    "home display connection closed before turn completion"
                );
            }
        }
        other => panic!("expected transport warning, not an error: {other:?}"),
    }
    assert_eq!(turn.as_deref(), Some("new-turn"));
    assert_eq!(execution, None);
    server.await.unwrap();
    client.shutdown().await;
    assert!(rx.try_recv().is_err());
    assert!(raw_rx.try_recv().is_err());
}

#[test]
fn forward_home_nonagent_has_no_execution() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx: mpsc::unbounded_channel().0,
    };
    forward_home(
        &out,
        json!({"type":"stream_chunk", "text":"answer", "stage_type":"shell",
        "run_id":"home", "stage_run_id":"exec", "nonce":7, "turn":"old-turn"}),
        "home",
        Some("submitted-turn"),
    );
    let (event, turn, execution) = rx.try_recv().unwrap();
    assert_eq!(serde_json::to_value(event).unwrap()["type"], "stream_chunk");
    assert_eq!(turn.as_deref(), Some("old-turn"));
    assert_eq!(execution, None);
    assert!(rx.try_recv().is_err());
}

#[test]
fn forward_home_history_gap_warns() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx: mpsc::unbounded_channel().0,
    };
    forward_home(
        &out,
        json!({"type":"history_gap", "turn":"old-turn"}),
        "home",
        Some("submitted-turn"),
    );
    let (event, turn, execution) = rx.try_recv().unwrap();
    match event {
        EventKind::SystemWarning { turn, message } => {
            assert_eq!(turn, None);
            assert_eq!(
                message,
                "home display history discontinuity: output may be missing"
            );
        }
        other => panic!("expected history warning, got {other:?}"),
    }
    assert_eq!(turn.as_deref(), Some("old-turn"));
    assert_eq!(execution, None);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn turnless_transport_gaps_render_only_in_the_submitted_turn() {
    for kind in ["history_gap", "lagged"] {
        for buffered in [true, false] {
            for reset in [false, true] {
                exercise_queued_gap(kind, buffered, reset).await;
            }
        }
    }
}

async fn exercise_queued_gap(kind: &str, buffered: bool, reset: bool) {
    use super::super::super::{
        home_events::apply_home_event, App, HomeInterrupt, LiveArea, Target,
    };
    use ratatui::backend::TestBackend;

    let (stream, peer) = UnixStream::pair().unwrap();
    let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![stream],
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    client.out = Arc::new(OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx: mpsc::unbounded_channel().0,
    });
    let gap = json!({"type":kind});
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut observer = watch(peer, true).await;
        if buffered {
            socket::write_json_line(observer.get_mut(), &gap)
                .await
                .unwrap();
        }

        let mut session = BufReader::new(session);
        let request = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(request["turn"], "1");
        socket::write_json_line(session.get_mut(), &json!({"type":"ok", "id":"home"}))
            .await
            .unwrap();
        if !buffered {
            socket::write_json_line(observer.get_mut(), &gap)
                .await
                .unwrap();
        }
        let _ = held.await;
    });
    client
        .send_session_request_for_generation(
            json!({"op":"chat", "text":"old prompt", "turn":"1"}),
            0,
        )
        .await
        .unwrap();
    // Leave the warning in the actual transport queue until after the UI reset.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while rx.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let mut app = App::new();
    let mut area = LiveArea::with_backend(TestBackend::new(100, 24), 100, 24);
    let mut interrupt = HomeInterrupt::default();
    app.conversations.begin_home_turn("1".into());
    area.submit_prompt("old prompt".into());
    if reset {
        area.clear_current_turn();
        area.clear_transcript();
        app.conversations.retire_home_session();
        interrupt = HomeInterrupt::default();
        // /clear increments turn_gen; the next submission increments it again.
        app.conversations.begin_home_turn("3".into());
        area.submit_prompt("new prompt".into());
    }
    let (event, turn, execution) = rx.try_recv().unwrap();
    assert!(matches!(event, EventKind::SystemWarning { .. }));
    assert_eq!(turn.as_deref(), Some("1"));
    apply_home_event(&mut app, &mut area, &mut interrupt, event, turn, execution).unwrap();
    assert!(area.target_has_current_turn(&Target::Home));
    area.compose(&app).unwrap();
    let rendered = area
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(
        rendered.contains("home display history discontinuity"),
        !reset,
        "kind={kind}, buffered={buffered}, reset={reset}: {rendered}"
    );
    assert!(rendered.contains(if reset { "new prompt" } else { "old prompt" }));
    release.send(()).unwrap();
    server.await.unwrap();
    client.shutdown().await;
}

#[tokio::test]
async fn home_enqueue_replacement_updates_identity_and_replays_agent_execution() {
    exercise_replacement(false).await;
}

#[tokio::test]
async fn home_enqueue_replacement_selected_history_gap_warns() {
    exercise_replacement(true).await;
}

async fn exercise_replacement(selected_gap: bool) {
    let (a, a_peer) = UnixStream::pair().unwrap();
    let (b, b_peer) = UnixStream::pair().unwrap();
    let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![a, b],
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    client.out = Arc::new(OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx,
    });
    let server = tokio::spawn(async move {
        let mut initial = watch(a_peer, true).await;
        socket::write_json_line(initial.get_mut(), &json!({"type":"lagged"}))
            .await
            .unwrap();
        socket::write_json_line(
            initial.get_mut(),
            &json!({"type":"stream_chunk",
            "text":"discarded stream", "run_id":"home", "correlation_turn":"new-turn"}),
        )
        .await
        .unwrap();
        let mut session = BufReader::new(session);
        let req = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(req["op"], "chat");
        assert_eq!(req["turn"], "new-turn");
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"ok", "id":"replacement", "status":"streaming"}),
        )
        .await
        .unwrap();
        let mut replacement = watch_run(b_peer, "replacement", true, true).await;
        if selected_gap {
            socket::write_json_line(
                replacement.get_mut(),
                &json!({"type":"history_gap", "correlation_turn":"new-turn"}),
            )
            .await
            .unwrap();
        }
        // Replay must carry execution identity without a start or capabilities frame.
        for frame in [
            json!({"type":"stream_chunk", "text":"answer", "run_id":"replacement",
                "stage_type":"agent", "stage_run_id":"exec", "correlation_stage":"chat", "sequence":1,
                "turn":"old-turn", "correlation_turn":"new-turn"}),
            json!({"type":"done", "text":"answer", "summary":"", "run_id":"replacement",
                "stage_type":"agent", "stage_run_id":"exec", "correlation_stage":"chat", "sequence":2,
                "turn":"old-turn", "correlation_turn":"new-turn"}),
        ] {
            socket::write_json_line(replacement.get_mut(), &frame)
                .await
                .unwrap();
        }
    });
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.send_session_request_for_generation(
            json!({"op":"chat", "text":"hi", "turn":"new-turn"}),
            0,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response["id"], "replacement");
    assert_eq!(client.home_run_id().await, "replacement");
    let identity = tokio::time::timeout(std::time::Duration::from_secs(2), raw_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        identity,
        json!({"type":"home_run_identity", "id":"replacement",
        "turn":"new-turn", "session_id":"owner", "generation":0})
    );
    if selected_gap {
        let (event, turn, execution) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        match event {
            EventKind::SystemWarning { turn, message } => {
                assert_eq!(turn, None);
                assert_eq!(
                    message,
                    "home display history discontinuity: output may be missing"
                );
            }
            other => panic!("expected selected history warning, got {other:?}"),
        }
        assert_eq!(turn.as_deref(), Some("new-turn"));
        assert_eq!(execution, None);
    }
    for expected in ["stream_chunk", "done"] {
        let (event, turn, execution) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(serde_json::to_value(event).unwrap()["type"], expected);
        assert_eq!(turn.as_deref(), Some("new-turn"));
        assert_eq!(
            execution,
            Some(Execution {
                run_id: "replacement".into(),
                stage_run_id: "exec".into(),
                nonce: 0,
            })
        );
    }
    server.await.unwrap();
    client.shutdown().await;
    assert!(rx.try_recv().is_err());
}
