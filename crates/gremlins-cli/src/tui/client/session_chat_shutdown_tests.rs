use super::*;
use std::time::Duration;
use tokio::net::UnixStream;

#[tokio::test]
async fn shutdown_retires_pending_replacement_watch_before_late_ack() {
    let (initial, initial_peer) = UnixStream::pair().unwrap();
    // Replacement is the acknowledged-ID replay acquisition, not a recovery retry.
    let (replacement, replacement_peer) = UnixStream::pair().unwrap();
    let (spare, spare_peer) = UnixStream::pair().unwrap();
    let (mut client, [_control, session]) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "chat_run_id":"G"}),
        vec![initial, replacement, spare],
    );
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
    client.out = Arc::new(OutState {
        chat_tx,
        raw_tx,
        event_tx: mpsc::unbounded_channel().0,
    });
    let (waiting_tx, waiting_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = async move {
        let mut initial = BufReader::new(initial_peer);
        let request = socket::read_json_line(&mut initial).await.unwrap().unwrap();
        assert_eq!(
            request,
            json!({"op":"watch", "scope":"run", "id":"G",
            "replay":false, "session_id":"owner"})
        );
        socket::write_json_line(
            initial.get_mut(),
            &json!({"type":"watched",
            "scope":"run", "id":"G", "stage":null, "stage_run_id":null, "replay":false}),
        )
        .await
        .unwrap();
        let mut session = BufReader::new(session);
        let request = socket::read_json_line(&mut session).await.unwrap().unwrap();
        assert_eq!(request["op"], "chat");
        assert_eq!(request["turn"], "old-turn");
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"ok", "chat_run_id":"H", "enqueued":true,
            "session_id":"owner"}),
        )
        .await
        .unwrap();
        let mut replacement = BufReader::new(replacement_peer);
        let request = socket::read_json_line(&mut replacement)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request,
            json!({"op":"watch", "scope":"run", "id":"H",
            "replay":true, "session_id":"owner"})
        );
        waiting_tx.send(()).unwrap();
        release_rx.await.unwrap();
        // One write makes the late ack and output available together.
        let frames = [
            json!({"type":"watched", "scope":"run", "id":"H",
                "stage":null, "stage_run_id":null, "replay":true}),
            json!({"type":"stream_chunk", "run_id":"H", "turn":"old-turn", "text":"retired"}),
            json!({"type":"done", "run_id":"H", "turn":"old-turn", "text":"retired", "summary":""}),
        ];
        let wire: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
        tokio::io::AsyncWriteExt::write_all(replacement.get_mut(), wire.as_bytes())
            .await
            .unwrap();
        // Keep the peer open: only client retirement can cause this closure.
        let closed = socket::read_json_line(&mut replacement).await;
        assert!(
            matches!(closed, Ok(None) | Err(_)),
            "unexpected client write: {closed:?}"
        );
    };
    let shutdown = async {
        waiting_rx.await.unwrap();
        assert!(client.session_chat_display.lock().await.is_none());
        assert_eq!(client.chat_run_id().await, "H");
        assert_eq!(
            raw_rx.recv().await.unwrap(),
            json!({"type":"session_chat_run_identity",
            "id":"H", "turn":"old-turn", "session_id":"owner", "generation":0})
        );
        client.shutdown().await;
        assert_eq!(client.session_generation().await, 1);
        release_tx.send(()).unwrap();
    };
    let send = client.send_session_request_for_generation(
        json!({"op":"chat", "text":"hi", "turn":"old-turn"}),
        0,
    );
    let (result, (), ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(send, shutdown, server)
    })
    .await
    .unwrap();
    assert_eq!(
        result.unwrap_err(),
        "session_chat display belongs to a retired session"
    );
    assert!(client.session_chat_display.lock().await.is_none());
    // Wait beyond the initial recovery backoff; no retry may acquire the spare socket.
    let mut spare_peer = BufReader::new(spare_peer);
    assert!(tokio::time::timeout(
        Duration::from_millis(200),
        socket::read_json_line(&mut spare_peer)
    )
    .await
    .is_err());
    assert_eq!(client.aux_streams.lock().await.as_ref().unwrap().len(), 1);
    assert!(
        chat_rx.try_recv().is_err(),
        "retired output or warning forwarded"
    );
    assert!(raw_rx.try_recv().is_err(), "late identity forwarded");
    assert!(client.session_chat_display.lock().await.is_none());
}
