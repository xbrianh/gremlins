use super::*;
use tokio::time::{timeout, Duration};

#[test]
fn disarmed_submission_does_not_clear_newer_pending_turn() {
    let gate = Arc::new(std::sync::Mutex::new(WatchGate {
        pending: true,
        turn: Some("A".into()),
        ..WatchGate::default()
    }));
    let changed = Arc::new(tokio::sync::Notify::new());
    let mut old = Submission(Arc::clone(&gate), changed, None, true);
    old.3 = false;
    {
        let mut state = gate.lock().unwrap();
        state.turn = Some("B".into());
        state
            .frames
            .push_back(json!({"type":"stream_chunk", "turn":"B", "text":"new"}));
    }

    drop(old);

    let state = gate.lock().unwrap();
    assert!(state.pending);
    assert_eq!(state.turn.as_deref(), Some("B"));
    assert_eq!(state.rejected, None);
    assert_eq!(state.frames.len(), 1);
    assert_eq!(state.frames[0]["text"], "new");
}

#[test]
fn rejection_removes_turn_ended_and_chunks_but_retains_run_ended() {
    let run_ended = json!({"type":"ended", "run_id":"home"});
    let previous_chunk = json!({"type":"stream_chunk", "run_id":"home", "turn":"A", "text":"keep"});
    let gate = Arc::new(std::sync::Mutex::new(WatchGate {
        pending: true,
        turn: Some("B".into()),
        frames: VecDeque::from([
            json!({"type":"stream_chunk", "run_id":"home", "turn":"B", "text":"discard"}),
            json!({"type":"ended", "run_id":"home", "correlation_turn":"B"}),
            run_ended.clone(),
            previous_chunk.clone(),
        ]),
        ..WatchGate::default()
    }));

    drop(Submission(
        Arc::clone(&gate),
        Arc::new(tokio::sync::Notify::new()),
        Some("A".into()),
        true,
    ));

    let state = gate.lock().unwrap();
    assert!(!state.pending);
    assert_eq!(state.turn.as_deref(), Some("A"));
    assert_eq!(state.rejected.as_deref(), Some("B"));
    assert_eq!(state.frames, VecDeque::from([run_ended, previous_chunk]));
}

#[tokio::test]
async fn initial_replacement_replay_warning_stays_with_completed_turn_until_checkpoint() {
    timeout(Duration::from_secs(5), async {
        let (client, _peers) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![],
        );
        let gate = Arc::new(std::sync::Mutex::new(WatchGate {
            acknowledged: true,
            turn: Some("A".into()),
            ..WatchGate::default()
        }));
        let changed = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = mpsc::channel(16);
        let (chat_tx, mut output) = mpsc::unbounded_channel();
        let forward = tokio::spawn(forward_home_gated(
            rx,
            Reader(tokio::spawn(std::future::pending())),
            HomeWatchContext {
                turn: Some("A".into()),
                ..review_context(&client, chat_tx)
            },
            None,
            Some((Arc::clone(&gate), Arc::clone(&changed))),
            || async { panic!("open replacement stream must not recover") },
        ));
        tx.send(json!({"type":"done", "run_id":"home", "turn":"A", "text":"done", "summary":""}))
            .await
            .unwrap();
        let (event, turn, _) = output.recv().await.unwrap();
        assert_eq!(serde_json::to_value(event).unwrap()["type"], "done");
        assert_eq!(turn.as_deref(), Some("A"));
        {
            let mut state = gate.lock().unwrap();
            assert_eq!(state.turn, None);
            state.turn = Some("B".into());
            state.pending = true;
        }
        for frame in [
            json!({"type":"history_gap"}),
            json!({"type":"replay_checkpoint"}),
            json!({"type":"lagged"}),
            json!({"type":"stream_chunk", "run_id":"home", "turn":"B", "text":"barrier"}),
        ] {
            tx.send(frame).await.unwrap();
        }
        loop {
            if gate.lock().unwrap().frames.len() == 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(output.try_recv().is_err());
        gate.lock().unwrap().pending = false;
        changed.notify_one();

        let (event, turn, _) = output.recv().await.unwrap();
        assert!(matches!(event, EventKind::SystemWarning { .. }));
        assert_eq!(turn.as_deref(), Some("A"));
        let (event, turn, _) = output.recv().await.unwrap();
        assert!(matches!(event, EventKind::SystemWarning { .. }));
        assert_eq!(turn.as_deref(), Some("B"));
        let (event, turn, _) = output.recv().await.unwrap();
        assert_eq!(serde_json::to_value(event).unwrap()["text"], "barrier");
        assert_eq!(turn.as_deref(), Some("B"));
        assert!(output.try_recv().is_err());
        forward.abort();
        assert!(forward.await.unwrap_err().is_cancelled());
        client.shutdown().await;
    })
    .await
    .expect("replacement replay warning test timed out");
}

#[tokio::test]
async fn failed_idle_recovery_retires_forward_task() {
    let (client, _peers) = DaemonClient::fake_with_aux_streams(
        json!({"session_id":"owner", "home_run_id":"home"}),
        vec![],
    );
    let gate = Arc::new(std::sync::Mutex::new(WatchGate {
        acknowledged: true,
        turn: None,
        ..WatchGate::default()
    }));
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    let (chat_tx, mut output) = mpsc::unbounded_channel();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recover_attempts = Arc::clone(&attempts);
    let forward = tokio::spawn(forward_home_gated(
        rx,
        Reader(tokio::spawn(std::future::pending())),
        review_context(&client, chat_tx),
        None,
        Some((Arc::clone(&gate), Arc::new(tokio::sync::Notify::new()))),
        move || {
            recover_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err("idle watch unavailable".into()) }
        },
    ));
    timeout(Duration::from_secs(1), forward)
        .await
        .expect("failed idle recovery left the forward task running")
        .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    {
        let state = gate.lock().unwrap();
        assert!(!state.recovering);
        assert!(state.disconnected);
        assert!(!state.pending);
        assert_eq!(state.turn, None);
    }
    assert!(output.try_recv().is_err());
    client.shutdown().await;
}

fn review_context(
    client: &DaemonClient,
    chat_tx: mpsc::UnboundedSender<ChatEvent>,
) -> HomeWatchContext {
    HomeWatchContext {
        session_write: Arc::clone(&client.session.write),
        out: Arc::new(OutState {
            chat_tx,
            event_tx: mpsc::unbounded_channel().0,
            raw_tx: mpsc::unbounded_channel().0,
        }),
        generation: 0,
        run: "home".into(),
        turn: None,
    }
}
