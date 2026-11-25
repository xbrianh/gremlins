use super::*;
use tokio::net::UnixStream;

async fn watch(peer: UnixStream, live: bool) -> BufReader<UnixStream> {
    let mut peer = BufReader::new(peer);
    let req = socket::read_json_line(&mut peer).await.unwrap().unwrap();
    assert_eq!(req["op"], "watch");
    assert_eq!(req["session_id"], "owner");
    assert_eq!(req["replay"], true);
    let ack = if live {
        json!({"type":"watched", "scope":"run", "id":"home", "stage":null, "stage_run_id":null, "replay":true})
    } else {
        json!({"type":"error", "message":"not live"})
    };
    socket::write_json_line(peer.get_mut(), &ack).await.unwrap();
    peer
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
    // The fixture's receivers are intentionally discarded; observe via a new
    // output channel on the same request/transport implementation.
    let mut client = client;
    client.out = Arc::new(OutState {
        chat_tx: tx,
        event_tx: mpsc::unbounded_channel().0,
        raw_tx: mpsc::unbounded_channel().0,
    });
    let server = tokio::spawn(async move {
        let mut observer = watch(a_peer, !first).await;
        if !first {
            socket::write_json_line(
                observer.get_mut(),
                &json!({"type":"stream_chunk", "text":"old", "run_id":"home"}),
            )
            .await
            .unwrap();
            socket::write_json_line(observer.get_mut(), &json!({"type":"replay_checkpoint"}))
                .await
                .unwrap();
        }
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
            observer = watch(b_peer, true).await;
        }
        for frame in [
            json!({"type":"turn_started", "stage_run_id":"execution", "run_id":"home"}),
            json!({"type":"stream_chunk", "text":"child", "run_id":"home/child"}),
            json!({"type":"stream_chunk", "text":"answer", "run_id":"home"}),
            json!({"type":"done", "text":"answer", "summary":"", "run_id":"home"}),
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
    for expected in ["turn_started", "stream_chunk", "done"] {
        let (event, turn, execution) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(serde_json::to_value(event).unwrap()["type"], expected);
        assert_eq!(turn.as_deref(), Some("new-turn"));
        assert!(
            execution.is_none(),
            "passive display must not grant control"
        );
    }
    server.await.unwrap();
    client.shutdown().await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn home_display_subscribes_before_enqueue_and_discards_previous_history() {
    exercise(false).await;
}

#[tokio::test]
async fn first_home_turn_uses_replay_after_ack_only_submission() {
    exercise(true).await;
}
