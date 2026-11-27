use super::*;
use std::time::Duration;
use tokio::net::UnixStream;

#[tokio::test]
async fn rejected_session_chat_reservation_does_not_block_next_enqueue_for_same_id() {
    rejected_reservation("reserved", false).await;
}

#[tokio::test]
async fn rejected_session_chat_reservation_does_not_block_next_enqueue_for_replacement_id() {
    rejected_reservation("replacement", false).await;
}

#[tokio::test]
async fn rejected_session_chat_reservation_retries_live_watch_and_captures_early_output() {
    rejected_reservation("reserved", true).await;
}

async fn rejected_reservation(acknowledged: &str, healthy_retry: bool) {
    let (initial, initial_peer) = UnixStream::pair().unwrap();
    let (retry, retry_peer) = UnixStream::pair().unwrap();
    let (replay, replay_peer) = UnixStream::pair().unwrap();
    let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "chat_run_id":"reserved"}),
        vec![initial],
    );
    let (chat_tx, mut output) = mpsc::unbounded_channel();
    let (raw_tx, mut identities) = mpsc::unbounded_channel();
    client.out = Arc::new(OutState {
        chat_tx,
        raw_tx,
        event_tx: mpsc::unbounded_channel().0,
    });
    let streams = Arc::clone(&client.aux_streams);
    let (first_received_tx, first_received_rx) = oneshot::channel();
    let (reject_tx, reject_rx) = oneshot::channel();
    let (replayed_tx, replayed_rx) = oneshot::channel();
    let (close_tx, close_rx) = oneshot::channel();
    let (early_tx, early_rx) = oneshot::channel();
    let (ack_tx, ack_rx) = oneshot::channel();
    let server = async move {
        let mut initial = BufReader::new(initial_peer);
        assert_eq!(
            socket::read_json_line(&mut initial).await.unwrap().unwrap(),
            json!({"op":"watch", "scope":"run", "id":"reserved",
                "session_id":"owner", "replay":false})
        );
        socket::write_json_line(
            initial.get_mut(),
            &json!({"type":"error", "message":"reservation is not live"}),
        )
        .await
        .unwrap();
        drop(initial);
        let mut session = BufReader::new(session);
        let first = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(first["op"], "chat");
        assert_eq!(first["turn"], "rejected-turn");
        first_received_tx.send(()).unwrap();
        reject_rx.await.unwrap();
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"error", "message":"enqueue rejected"}),
        )
        .await
        .unwrap();

        // The next prompt must acquire a fresh live-only watch before enqueue.
        // The rejected reservation must not enter replay recovery.
        let mut retry = BufReader::new(retry_peer);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), socket::read_json_line(&mut retry))
                .await
                .expect("next prompt did not retry the live-only watch before enqueue")
                .unwrap()
                .unwrap(),
            json!({"op":"watch", "scope":"run", "id":"reserved",
                "session_id":"owner", "replay":false})
        );
        let retry_ack = if healthy_retry {
            json!({"type":"watched", "scope":"run", "id":"reserved",
                "stage":null, "stage_run_id":null, "replay":false})
        } else {
            json!({"type":"error", "message":"reservation is not live"})
        };
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                socket::read_json_line(&mut session)
            )
            .await
            .is_err(),
            "next prompt enqueued before the live-only watch handshake completed"
        );
        socket::write_json_line(retry.get_mut(), &retry_ack)
            .await
            .unwrap();
        let mut retry = Some(retry);
        if !healthy_retry {
            retry.take();
        }
        // No replay transport exists until the next enqueue reaches the daemon.
        let next =
            tokio::time::timeout(Duration::from_secs(1), socket::read_json_line(&mut session))
                .await
                .expect("next enqueue waited for replay of a rejected reservation")
                .unwrap()
                .unwrap();
        assert_eq!(next["op"], "chat");
        assert_eq!(next["turn"], "accepted-turn");
        if healthy_retry {
            for frame in [
                json!({"type":"stream_chunk", "run_id":acknowledged,
                    "turn":"accepted-turn", "text":"early answer"}),
                json!({"type":"done", "run_id":acknowledged,
                    "turn":"accepted-turn", "text":"early answer", "summary":""}),
            ] {
                socket::write_json_line(retry.as_mut().unwrap().get_mut(), &frame)
                    .await
                    .unwrap();
            }
            early_tx.send(()).unwrap();
            ack_rx.await.unwrap();
        } else {
            streams.lock().await.as_mut().unwrap().push_back(replay);
        }
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"ok", "chat_run_id":acknowledged, "enqueued":true, "session_id":"owner"}),
        )
        .await
        .unwrap();
        let mut replay = BufReader::new(replay_peer);
        if !healthy_retry {
            assert_eq!(
                socket::read_json_line(&mut replay).await.unwrap().unwrap(),
                json!({"op":"watch", "scope":"run", "id":acknowledged,
                "session_id":"owner", "replay":true})
            );
            for frame in [
                json!({"type":"watched", "scope":"run", "id":acknowledged,
                "stage":null, "stage_run_id":null, "replay":true}),
                json!({"type":"stream_chunk", "run_id":acknowledged,
                "turn":"accepted-turn", "sequence":1, "text":"replayed answer"}),
                json!({"type":"done", "run_id":acknowledged, "turn":"accepted-turn",
                "sequence":2, "text":"replayed answer", "summary":""}),
            ] {
                socket::write_json_line(replay.get_mut(), &frame)
                    .await
                    .unwrap();
            }
        }
        replayed_tx.send(()).unwrap();
        close_rx.await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                socket::read_json_line(&mut session)
            )
            .await
            .is_err(),
            "recovery resubmitted a chat request"
        );
        if healthy_retry {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(30),
                    socket::read_json_line(&mut replay)
                )
                .await
                .is_err(),
                "healthy live-only retry unexpectedly requested replay"
            );
            assert!(streams.lock().await.as_ref().unwrap().is_empty());
        }
        // Keep watch peers open until assertions finish, avoiding another EOF.
    };
    let driver = async {
        let first = client.send_session_request_for_generation(
            json!({"op":"chat", "text":"first", "turn":"rejected-turn"}),
            0,
        );
        let reject_after_eof = async {
            first_received_rx.await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let eof_processed = client
                        .session_chat_display
                        .lock()
                        .await
                        .as_ref()
                        .is_some_and(|display| {
                            let gate = display.gate.lock().unwrap();
                            gate.pending && gate.disconnected && !gate.recovering
                        });
                    if eof_processed {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("failed initial watch EOF was not processed while enqueue was pending");
            tokio::task::yield_now().await;
            assert!(output.try_recv().is_err(), "unacknowledged output escaped");
            reject_tx.send(()).unwrap();
        };
        let (first, ()) = tokio::join!(first, reject_after_eof);
        assert_eq!(first.unwrap()["type"], "error");
        assert!(
            identities.try_recv().is_err(),
            "rejection published an identity"
        );
        {
            let display = client.session_chat_display.lock().await;
            let display = display.as_ref().unwrap();
            let gate = display.gate.lock().unwrap();
            assert!(gate.disconnected, "failed watch remained connected");
            assert!(!gate.recovering, "rejected reservation entered recovery");
            assert!(!gate.pending);
            assert!(!gate.acknowledged);
        }
        client
            .aux_streams
            .lock()
            .await
            .as_mut()
            .unwrap()
            .push_back(retry);
        let accepted = client.send_session_request_for_generation(
            json!({"op":"chat", "text":"second", "turn":"accepted-turn"}),
            0,
        );
        let check_early_output = async {
            if healthy_retry {
                early_rx.await.unwrap();
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        let buffered = {
                            let display = client.session_chat_display.lock().await;
                            let gate = display.as_ref().unwrap().gate.lock().unwrap();
                            gate.pending && gate.frames.iter().any(|frame| frame["type"] == "done")
                        };
                        if buffered {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("live-only retry did not capture early output");
                assert!(
                    output.try_recv().is_err(),
                    "output escaped before acknowledgment"
                );
                assert!(identities.try_recv().is_err());
                ack_tx.send(()).unwrap();
            }
        };
        let (accepted, ()) = tokio::join!(accepted, check_early_output);
        let accepted = accepted.unwrap();
        assert_eq!(accepted["type"], "ok");
        assert_eq!(accepted["chat_run_id"], acknowledged);
        assert_eq!(accepted["enqueued"], true);
        assert_eq!(client.chat_run_id().await, acknowledged);
        assert_eq!(
            identities.recv().await.unwrap(),
            json!({"type":"session_chat_run_identity", "id":acknowledged,
                "turn":"accepted-turn", "session_id":"owner", "generation":0})
        );
        replayed_rx.await.unwrap();
        for expected in ["stream_chunk", "done"] {
            let (event, turn, execution) = output.recv().await.unwrap();
            assert_ne!(turn.as_deref(), Some("rejected-turn"));
            if matches!(event, EventKind::SystemWarning { .. }) {
                panic!("successful acquisition emitted a spurious warning: {event:?}");
            }
            assert_eq!(turn.as_deref(), Some("accepted-turn"));
            assert_eq!(execution, None);
            let event = serde_json::to_value(event).unwrap();
            assert_eq!(event["type"], expected);
            assert_eq!(
                event["text"],
                if healthy_retry {
                    "early answer"
                } else {
                    "replayed answer"
                }
            );
        }
        assert!(
            output.try_recv().is_err(),
            "unexpected duplicate replay output"
        );
        assert!(identities.try_recv().is_err());
        close_tx.send(()).unwrap();
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(server, driver);
    })
    .await
    .expect("reservation regression scenario timed out");
    client.shutdown().await;
}
