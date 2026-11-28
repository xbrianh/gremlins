use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::time::timeout;

fn connection() -> (mpsc::Sender<Value>, Opened, tokio::task::AbortHandle) {
    let (tx, rx) = mpsc::channel(1024);
    let task = tokio::spawn(std::future::pending());
    let abort = task.abort_handle();
    (tx, (rx, task), abort)
}

fn frame(sequence: u64) -> Value {
    json!({"type":"log", "id":"run", "sequence":sequence, "text":"same"})
}

async fn next(rx: &mut mpsc::Receiver<Value>) -> Value {
    timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn replay_dedups_sequences_not_content_and_does_not_resend() {
    let (tx, initial, _) = connection();
    let (replay_tx, replay, _) = connection();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut replay = Some(replay);
    let (mut watch, mut rx) = spawn("run".into(), initial, move || {
        count.fetch_add(1, Ordering::SeqCst);
        let opened = replay.take();
        async move { opened.ok_or_else(|| "unexpected extra reconnect".into()) }
    });
    tx.send(frame(1)).await.unwrap();
    tx.send(frame(2)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(1));
    assert_eq!(next(&mut rx).await, frame(2));
    drop(tx);
    for seq in [1, 2, 3, 3, 4] {
        replay_tx.send(frame(seq)).await.unwrap();
    }
    let unsequenced = json!({"type":"log", "text":"same"});
    replay_tx.send(unsequenced.clone()).await.unwrap();
    replay_tx.send(unsequenced.clone()).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(3));
    assert_eq!(next(&mut rx).await, frame(4));
    assert_eq!(next(&mut rx).await, unsequenced);
    assert_eq!(next(&mut rx).await, unsequenced);
    settle().await;
    assert!(rx.try_recv().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    watch.cancel().await;
}

#[tokio::test]
async fn overflow_is_nonblocking_and_reports_gap_without_another_frame() {
    let (tx, initial, _) = connection();
    let (mut watch, mut rx) = spawn("run".into(), initial, || async { Err("offline".into()) });
    for seq in 1..=300 {
        tx.send(frame(seq)).await.unwrap();
    }
    settle().await;
    // All input was consumed despite a completely full outgoing queue.
    assert_eq!(tx.capacity(), 1024);
    for seq in 1..=256 {
        assert_eq!(next(&mut rx).await, frame(seq));
    }
    let gap = next(&mut rx).await;
    assert_eq!(gap["type"], "history_gap");
    assert!(gap["message"].as_str().unwrap().contains("overflow"));
    settle().await;
    assert!(rx.try_recv().is_err());
    // Dropped frames did not advance the cursor.
    tx.send(frame(257)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(257));
    watch.cancel().await;
}

#[tokio::test]
async fn cancel_and_drop_abort_owned_reader() {
    let (_tx, initial, abort) = connection();
    let (mut watch, mut rx) = spawn("run".into(), initial, || async { Err("offline".into()) });
    watch.cancel().await;
    assert!(
        abort.is_finished(),
        "cancel must join even an unpolled reader"
    );
    watch.cancel().await;
    assert!(abort.is_finished());
    assert!(rx.recv().await.is_none());

    let (_tx, initial, abort) = connection();
    let (watch, mut rx) = spawn("run".into(), initial, || async { Err("offline".into()) });
    drop(watch);
    settle().await;
    assert!(abort.is_finished());
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn cancel_joins_started_reader_before_returning() {
    struct Finished(Arc<AtomicUsize>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.store(1, Ordering::SeqCst);
        }
    }
    let finished = Arc::new(AtomicUsize::new(0));
    let guard = Finished(finished.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let reader = tokio::spawn(async move {
        let _guard = guard;
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    started_rx.await.unwrap();
    let (_tx, rx) = mpsc::channel(1);
    let (mut watch, _rx) = spawn("run".into(), (rx, reader), || async {
        Err("offline".into())
    });
    watch.cancel().await;
    assert_eq!(finished.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn receiver_drop_cleans_up_reader() {
    let (_tx, initial, abort) = connection();
    let (mut watch, rx) = spawn("run".into(), initial, || async { Err("offline".into()) });
    drop(rx);
    settle().await;
    assert!(watch.task.as_ref().unwrap().is_finished());
    assert!(abort.is_finished());
    watch.cancel().await;
}

#[tokio::test]
async fn only_matching_run_terminal_stops_without_reopening() {
    for kind in ["run_completed", "run_failed", "run_stopped", "ended"] {
        let (tx, initial, abort) = connection();
        let (mut watch, mut rx) = spawn("run".into(), initial, || async {
            panic!("terminal must not reconnect");
            #[allow(unreachable_code)]
            Err("unexpected".into())
        });
        let foreign = json!({"type":kind,"id":"other"});
        tx.send(foreign.clone()).await.unwrap();
        tx.send(frame(1)).await.unwrap();
        for key in ["correlation_turn", "turn", "chat_turn"] {
            let mut ended = json!({"type":"ended", "id":"run"});
            ended[key] = json!(1);
            tx.send(ended.clone()).await.unwrap();
        }
        let terminal = json!({"type":kind,"run_id":"run"});
        tx.send(terminal.clone()).await.unwrap();
        assert_eq!(next(&mut rx).await, foreign);
        assert_eq!(next(&mut rx).await, frame(1));
        for key in ["correlation_turn", "turn", "chat_turn"] {
            let ended = next(&mut rx).await;
            assert_eq!(ended[key], 1);
        }
        assert_eq!(next(&mut rx).await, terminal);
        assert!(timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .is_none());
        settle().await;
        assert!(abort.is_finished());
        watch.cancel().await;
    }
}

#[test]
fn ended_requires_matching_run_and_no_turn_alias() {
    for id_key in ["id", "run_id"] {
        let mut ended = json!({"type":"ended"});
        assert!(!is_terminal("run", &ended));
        ended[id_key] = json!("other");
        assert!(!is_terminal("run", &ended));
        ended[id_key] = json!("run");
        assert!(is_terminal("run", &ended));
        for key in ["correlation_turn", "turn", "chat_turn"] {
            for turn in [json!("1"), json!(1), json!(0), json!("")] {
                ended[key] = turn;
                assert!(!is_terminal("run", &ended));
            }
            ended[key] = Value::Null;
            assert!(is_terminal("run", &ended));
        }
    }
}

#[tokio::test]
async fn successful_first_retry_is_silent() {
    let (tx, initial, _) = connection();
    let (replay_tx, replay, _) = connection();
    let mut replay = Some(replay);
    let (mut watch, mut rx) = spawn("run".into(), initial, move || {
        let opened = replay.take();
        async move { opened.ok_or_else(|| "offline".into()) }
    });
    drop(tx);
    replay_tx.send(frame(1)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(1));
    watch.cancel().await;
}

#[tokio::test]
async fn first_recovery_closing_without_activity_warns_once() {
    for history in [
        vec![],
        vec![frame(1)],
        vec![json!({"type":"replay_checkpoint", "id":"run"})],
        vec![frame(1), json!({"type":"replay_checkpoint", "id":"run"})],
    ] {
        let (tx, initial, _) = connection();
        let (replay_tx, replay, _) = connection();
        let (again_tx, again, _) = connection();
        let (live_tx, live, _) = connection();
        let mut connections = std::collections::VecDeque::from([replay, again, live]);
        let (mut watch, mut rx) = spawn("run".into(), initial, move || {
            let opened = connections.pop_front();
            async move { opened.ok_or_else(|| "offline".into()) }
        });
        tx.send(frame(1)).await.unwrap();
        assert_eq!(next(&mut rx).await, frame(1));
        drop(tx);
        for value in &history {
            replay_tx.send(value.clone()).await.unwrap();
        }
        drop(replay_tx);
        // A second successful handshake without activity is the same outage.
        again_tx.send(frame(1)).await.unwrap();
        drop(again_tx);
        for value in &history {
            if value["type"] == "replay_checkpoint" {
                assert_eq!(next(&mut rx).await, *value);
            }
        }
        let warning = next(&mut rx).await;
        assert_eq!(warning["type"], "history_gap");
        assert!(warning["message"]
            .as_str()
            .unwrap()
            .contains("reader closed"));
        live_tx.send(frame(2)).await.unwrap();
        assert_eq!(next(&mut rx).await, frame(2));
        drop(live_tx);
        // Fresh activity makes a subsequent outage eligible for a warning.
        assert_eq!(next(&mut rx).await["type"], "history_gap");
        watch.cancel().await;
    }
}

#[tokio::test]
async fn failures_warn_once_per_outage_and_recover() {
    let (tx, initial, _) = connection();
    let (replay_tx, replay, _) = connection();
    let (last_tx, last, _) = connection();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut replay = Some(replay);
    let mut last = Some(last);
    let (mut watch, mut rx) = spawn("run".into(), initial, move || {
        let n = count.fetch_add(1, Ordering::SeqCst);
        let result = match n {
            2 => Ok(replay.take().unwrap()),
            4 => Ok(last.take().unwrap()),
            _ => Err("fake offline".into()),
        };
        async move { result }
    });
    drop(tx);
    let warning = next(&mut rx).await;
    assert_eq!(warning["type"], "history_gap");
    assert!(warning["message"]
        .as_str()
        .unwrap()
        .contains("fake offline"));
    replay_tx.send(frame(1)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(1));
    drop(replay_tx);
    assert_eq!(next(&mut rx).await["type"], "history_gap");
    last_tx.send(frame(2)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(2));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    watch.cancel().await;
}

#[tokio::test]
async fn replay_only_reconnect_does_not_reset_outage_warning() {
    let (tx, initial, _) = connection();
    let (replay_tx, replay, _) = connection();
    let (live_tx, live, _) = connection();
    let mut replay = Some(replay);
    let mut live = Some(live);
    let mut calls = 0;
    let (mut watch, mut rx) = spawn("run".into(), initial, move || {
        let result = match calls {
            1 => Ok(replay.take().unwrap()),
            3 => Ok(live.take().unwrap()),
            _ => Err("offline".into()),
        };
        calls += 1;
        async move { result }
    });
    tx.send(frame(1)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(1));
    drop(tx);
    assert_eq!(next(&mut rx).await["type"], "history_gap");
    replay_tx.send(frame(1)).await.unwrap();
    let checkpoint = json!({"type":"replay_checkpoint", "id":"run"});
    replay_tx.send(checkpoint.clone()).await.unwrap();
    drop(replay_tx);
    assert_eq!(next(&mut rx).await, checkpoint);
    live_tx.send(frame(2)).await.unwrap();
    // No second warning for the replay-only connection's failure.
    assert_eq!(next(&mut rx).await, frame(2));
    drop(live_tx);
    // Fresh activity restores warning eligibility for the next outage.
    assert_eq!(next(&mut rx).await["type"], "history_gap");
    watch.cancel().await;
}

#[tokio::test]
async fn terminal_retained_on_overflow_follows_gap_and_stops() {
    let (tx, initial, abort) = connection();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (mut watch, mut rx) = spawn("run".into(), initial, move || {
        count.fetch_add(1, Ordering::SeqCst);
        async { Err("unexpected reconnect".into()) }
    });
    for seq in 1..=256 {
        tx.send(frame(seq)).await.unwrap();
    }
    let terminal = json!({"type":"ended", "id":"run", "sequence":257});
    tx.send(terminal.clone()).await.unwrap();
    settle().await;
    for seq in 1..=256 {
        assert_eq!(next(&mut rx).await, frame(seq));
    }
    assert_eq!(next(&mut rx).await["type"], "history_gap");
    assert_eq!(next(&mut rx).await, terminal);
    assert!(timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .is_none());
    settle().await;
    assert!(abort.is_finished());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    watch.cancel().await;
}

#[tokio::test]
async fn pending_gap_drops_bursts_without_advancing_cursor() {
    let (tx, initial, _) = connection();
    let (mut watch, mut rx) = spawn("run".into(), initial, || async { Err("offline".into()) });
    for seq in 1..=CAPACITY as u64 + 1 {
        tx.send(frame(seq)).await.unwrap();
    }
    settle().await;
    // Keep the gap pending while much more input arrives. No side queue grows.
    for seq in 1000..2000 {
        tx.send(frame(seq)).await.unwrap();
    }
    settle().await;
    assert_eq!(tx.capacity(), 1024);
    for seq in 1..=CAPACITY as u64 {
        assert_eq!(next(&mut rx).await, frame(seq));
    }
    assert_eq!(next(&mut rx).await["type"], "history_gap");
    settle().await;
    assert!(rx.try_recv().is_err());
    tx.send(frame(CAPACITY as u64 + 1)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(CAPACITY as u64 + 1));
    watch.cancel().await;
}

#[tokio::test]
async fn duplicate_terminal_does_not_stop_live_stream() {
    let (tx, initial, _) = connection();
    let (mut watch, mut rx) = spawn("run".into(), initial, || async { Err("offline".into()) });
    tx.send(frame(10)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(10));
    tx.send(json!({"type":"run_completed", "id":"run", "sequence":10}))
        .await
        .unwrap();
    tx.send(frame(11)).await.unwrap();
    assert_eq!(next(&mut rx).await, frame(11));
    let terminal = json!({"type":"run_completed", "id":"run", "sequence":12});
    tx.send(terminal.clone()).await.unwrap();
    assert_eq!(next(&mut rx).await, terminal);
    assert!(timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .is_none());
    watch.cancel().await;
}

#[tokio::test]
async fn cancellation_interrupts_pending_opener() {
    let (tx, initial, _) = connection();
    let (started_tx, mut started_rx) = mpsc::channel(1);
    let (mut watch, mut rx) = spawn("run".into(), initial, move || {
        let started = started_tx.clone();
        async move {
            started.send(()).await.unwrap();
            std::future::pending().await
        }
    });
    drop(tx);
    timeout(Duration::from_secs(1), started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(1), watch.cancel())
        .await
        .unwrap();
    assert!(rx.recv().await.is_none());
}
