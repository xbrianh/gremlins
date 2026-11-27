use super::*;
use tokio::net::UnixStream;
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn idle_live_lagged_cannot_be_adopted_by_next_turn() {
    exercise_idle_live_warning("lagged").await;
}

#[tokio::test]
async fn idle_live_history_gap_cannot_be_adopted_by_next_turn() {
    exercise_idle_live_warning("history_gap").await;
}

async fn exercise_idle_live_warning(kind: &str) {
    timeout(Duration::from_secs(5), async {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (client, _session_peers) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![],
        );
        // A has finished. No forwarder may drain the socket reader's queue
        // until B has become the active submission.
        let gate = Arc::new(std::sync::Mutex::new(WatchGate {
            turn: None,
            acknowledged: true,
            ..WatchGate::default()
        }));
        let marker_seen = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::clone(&marker_seen);
        let mut receive = home_frame_receiver(false, Some(Arc::clone(&gate)));
        let watch = follow_watch_mapped(stream, "owner", "home", None, None, false, move |frame| {
            let marker = frame["type"] == "stream_chunk" && frame["text"] == "idle marker";
            let mapped = receive(frame);
            if marker {
                seen.notify_one();
            }
            mapped
        });
        let (watched, mut peer) = tokio::join!(watch, acknowledge_live_watch(peer));
        let (rx, reader) = watched.unwrap();
        socket::write_json_line(peer.get_mut(), &json!({"type":kind}))
            .await
            .unwrap();
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"stream_chunk", "run_id":"home", "turn":"B", "text":"idle marker"}),
        )
        .await
        .unwrap();
        // The marker is behind the warning on the same socket. Observing it
        // proves ingress handled the idle warning, without consuming raw rx.
        marker_seen.notified().await;
        while rx.is_empty() {
            tokio::task::yield_now().await;
        }

        let changed = Arc::new(tokio::sync::Notify::new());
        {
            let mut state = gate.lock().unwrap();
            state.turn = Some("B".into());
            state.pending = true;
        }
        let (chat_tx, mut output) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_home_gated(
            rx,
            Reader(reader),
            HomeWatchContext {
                session_write: Arc::clone(&client.session.write),
                out: Arc::new(OutState {
                    chat_tx,
                    event_tx: mpsc::unbounded_channel().0,
                    raw_tx: mpsc::unbounded_channel().0,
                }),
                generation: 0,
                run: "home".into(),
                turn: Some("B".into()),
            },
            None,
            Some((Arc::clone(&gate), Arc::clone(&changed))),
            || async { panic!("peer remains open; recovery must not run") },
        ));
        gate.lock().unwrap().pending = false;
        changed.notify_one();

        // This output barrier also catches an idle warning wrongly retained in
        // raw rx: it would be forwarded before the marker and attributed to B.
        let (event, turn, _) = output.recv().await.unwrap();
        let event = serde_json::to_value(event).unwrap();
        assert_eq!(
            event["type"], "stream_chunk",
            "{kind}: idle warning leaked into B: {event}"
        );
        assert_eq!(event["text"], "idle marker");
        assert_eq!(turn.as_deref(), Some("B"));
        assert!(output.try_recv().is_err());

        // A genuine turnless live warning during B must still reach B.
        socket::write_json_line(peer.get_mut(), &json!({"type":kind}))
            .await
            .unwrap();
        let (event, turn, execution) = output.recv().await.unwrap();
        assert!(
            matches!(event, EventKind::SystemWarning { turn: None, ref message }
            if message == "home display history discontinuity: output may be missing")
        );
        assert_eq!(turn.as_deref(), Some("B"));
        assert_eq!(execution, None);
        assert!(output.try_recv().is_err());

        // Keep the peer alive through all assertions, avoiding EOF warnings.
        forward.abort();
        assert!(forward.await.unwrap_err().is_cancelled());
        drop(peer);
        client.shutdown().await;
    })
    .await
    .unwrap_or_else(|_| panic!("{kind}: live-warning regression test timed out"));
}

async fn acknowledge_live_watch(peer: UnixStream) -> BufReader<UnixStream> {
    let mut peer = BufReader::new(peer);
    let request = socket::read_json_line(&mut peer).await.unwrap().unwrap();
    assert_eq!(
        request,
        json!({"op":"watch", "scope":"run", "id":"home",
        "session_id":"owner", "replay":false})
    );
    socket::write_json_line(
        peer.get_mut(),
        &json!({"type":"watched", "scope":"run",
        "id":"home", "stage":null, "stage_run_id":null, "replay":false}),
    )
    .await
    .unwrap();
    peer
}
