use std::time::Duration;

use gremlins_magic_bus::connection::BusSubGuard;
use gremlins_magic_bus::{CorrId, DispatchEvent, SubscriberRegistration, Timestamp};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::super::lifecycle::setup_gremlin_channels;
use super::super::run_registry::LogFlushHandle;
use super::*;
use crate::clients::client::ModelURI;
use crate::executor::state::{stamp_from_epoch_millis, token_hex};
use crate::test_support::Sandbox;

const LIMIT: Duration = Duration::from_secs(15);

fn subscribe(
    id: &str,
) -> (
    Subscriber<EventKind>,
    gremlins_magic_bus::ReceiverHalf<EventKind>,
) {
    init_bus()
        .subscribe(SubscriberRegistration::lossless(CorrId::new().with_run(id)))
        .unwrap()
}

// Reuse one captured timestamp so retries are identical even after stamping.
fn emit_line(id: &str, line: &str, ts: Timestamp) {
    init_bus().emit(DispatchEvent::with_timestamp(
        CorrId::new().with_run(id),
        EventKind::LogLine {
            id: id.into(),
            line: line.into(),
        },
        ts,
    ));
}

fn stamped(line: &str, ts: Timestamp) -> String {
    let stamp = stamp_from_epoch_millis(ts.wall_millis());
    line.split('\n')
        .map(|part| format!("{stamp} {part}\n"))
        .collect()
}

async fn snapshot(flush: &LogFlushHandle) -> LogSnapshot {
    timeout(LIMIT, LogSnapshot::request(flush))
        .await
        .expect("snapshot timed out")
        .expect("writer dropped snapshot")
}

#[tokio::test]
async fn lifecycle_snapshot_preserves_identical_events_and_empty_fragments() {
    timeout(LIMIT, async {
        let sandbox = Sandbox::new();
        let id = format!("log-snapshot-{}", token_hex(8));
        let mut gremlin = super::super::chat::create_chat_gremlin(
            &id,
            &ModelURI::parse("cmd/echo").unwrap(),
            "helpful",
            sandbox.path(),
            &Default::default(),
        )
        .unwrap();
        let (_, flush, writer_sub) = setup_gremlin_channels(&mut gremlin, None);
        let _writer_guard = BusSubGuard::new(writer_sub);
        let (sub, mut rx) = subscribe(&id);
        let _guard = BusSubGuard::new(sub);
        let ts = Timestamp::now();
        let lines = ["retry", "retry", "first\n\nlast\n", "", "retry"];
        // No await between emitting and requesting the snapshot: the lifecycle
        // writer must drain the queued bus events before honoring its barrier.
        for line in lines {
            emit_line(&id, line, ts);
        }
        let cut = snapshot(&flush).await;
        let expected: String = lines.iter().map(|line| stamped(line, ts)).collect();
        assert_eq!(cut.content, expected);
        let mut last_sequence = 0;
        for _ in lines {
            let gremlins_magic_bus::Recv::Event(event) = rx.recv().await else {
                panic!("lossless subscriber lost an event");
            };
            assert!(event.sequence() > last_sequence);
            last_sequence = event.sequence();
        }
        assert_eq!(cut.through_sequence, last_sequence);
        assert!(cut.tail(0).is_empty());
        assert_eq!(cut.tail(usize::MAX), expected.lines().collect::<Vec<_>>());
        assert_eq!(
            cut.tail(1),
            vec![stamped("retry", ts).trim_end().to_owned()]
        );

        emit_line(&id, "retry", ts);
        let next = snapshot(&flush).await;
        assert_eq!(next.content, format!("{expected}{}", stamped("retry", ts)));
        assert!(next.through_sequence > cut.through_sequence);
    })
    .await
    .expect("lifecycle snapshot test timed out");
}

#[tokio::test]
async fn registry_snapshot_uses_persisted_log_events_not_bus_head() {
    timeout(LIMIT, async {
        let sandbox = Sandbox::new();
        let id = format!("log-checkpoint-{}", token_hex(8));
        let gremlin = super::super::chat::create_chat_gremlin(
            &id,
            &ModelURI::parse("cmd/echo").unwrap(),
            "helpful",
            sandbox.path(),
            &Default::default(),
        )
        .unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let registry = Arc::new(RunRegistry::new(shutdown));
        registry
            .register_chat_gremlin(
                id.clone(),
                gremlin,
                crate::clients::cancel::CancelToken::new(),
            )
            .await;
        let (sub, mut rx) = subscribe(&id);
        let _guard = BusSubGuard::new(sub);
        let empty = registry.get_run_log_snapshot(&id).await.unwrap();
        assert_eq!(empty.through_sequence, 0);
        let ts = Timestamp::now();
        emit_line(&id, "first\n\nlast\n", ts);
        let gremlins_magic_bus::Recv::Event(event) = rx.recv().await else {
            panic!("missing log event");
        };
        let sequence = event.sequence();
        // A descendant's raw event must not advance the parent's checkpoint.
        emit_line(&format!("{id}:child"), "not parent history", ts);
        let cut = registry.get_run_log_snapshot(&id).await.unwrap();
        assert_eq!(cut.through_sequence, sequence);
        assert_eq!(cut.content, stamped("first\n\nlast\n", ts));
        assert_eq!(
            cut.tail(2),
            vec![
                format!("{} last", stamp_from_epoch_millis(ts.wall_millis())),
                format!("{} ", stamp_from_epoch_millis(ts.wall_millis())),
            ]
        );
        let tail = registry.get_run_log_tail_snapshot(&id, 2).await.unwrap();
        assert_eq!(tail.tail(2), cut.tail(2));
        assert_eq!(tail.through_sequence, 0);
        let empty_tail = registry.get_run_log_tail_snapshot(&id, 0).await.unwrap();
        assert!(empty_tail.content.is_empty());
        assert_eq!(empty_tail.through_sequence, 0);
        // A tail request must not alter the full-snapshot cut used by log follow.
        let full = registry.get_run_log_snapshot(&id).await.unwrap();
        assert_eq!(full.content, cut.content);
        assert_eq!(full.through_sequence, sequence);

        let large_event = (0..20_000)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        emit_line(&id, &large_event, ts);
        let tail = registry.get_run_log_tail_snapshot(&id, 500).await.unwrap();
        let expected = (19_500..20_000)
            .map(|n| format!("{} line {n}", stamp_from_epoch_millis(ts.wall_millis())))
            .collect::<Vec<_>>();
        assert_eq!(tail.tail(usize::MAX), expected);
        assert_eq!(tail.through_sequence, 0);
        registry.stop_run(&id).await;
        assert!(registry.get_run_log_snapshot(&id).await.is_none());
    })
    .await
    .expect("registry snapshot test timed out");
}

#[tokio::test]
async fn blocked_replay_follows_identical_retry_and_drains_after_queued_terminal() {
    timeout(LIMIT, async {
        let sandbox = Sandbox::new();
        let id = format!("log-handoff-{}", token_hex(8));
        let mut gremlin = super::super::chat::create_chat_gremlin(
            &id,
            &ModelURI::parse("cmd/echo").unwrap(),
            "helpful",
            sandbox.path(),
            &Default::default(),
        )
        .unwrap();
        let (_, flush, writer_sub) = setup_gremlin_channels(&mut gremlin, None);
        let _writer_guard = BusSubGuard::new(writer_sub);
        let (sub, rx) = subscribe(&id);
        let _guard = BusSubGuard::new(sub.clone());
        let ts = Timestamp::now();
        // Far larger than the Unix socket buffer: replay cannot finish until
        // the client resumes reading. This is real lifecycle-written history.
        let large = "x".repeat(4 * 1024 * 1024);
        let retry = "retry\n\nretry\n";
        emit_line(&id, &large, ts);
        emit_line(&id, retry, ts);
        emit_line(&id, retry, ts);
        let cut = snapshot(&flush).await;
        let expected_history = format!(
            "{}{}{}",
            stamped(&large, ts),
            stamped(retry, ts),
            stamped(retry, ts)
        );
        assert_eq!(cut.content, expected_history);
        assert!(cut.through_sequence > 0);
        let history_lines = cut.content.lines().count() as u64;

        let (mut client, server) = UnixStream::pair().unwrap();
        let (reader, writer) = server.into_split();
        let writer = SharedWriter::new(writer);
        let follow_id = id.clone();
        let (replayed_tx, mut replayed_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let log_sub = Some(sub);
            let out = LogOutput {
                writer: &writer,
                log_sub: &log_sub,
            };
            let mut offset = replay_history(&cut.content, &out)
                .await
                .expect("replay disconnected");
            assert_eq!(offset, history_lines);
            replayed_tx.send(()).unwrap();
            drain_and_follow(
                &follow_id,
                rx,
                &out,
                BufReader::new(reader),
                cut.through_sequence,
                &mut offset,
            )
            .await;
            offset
        });

        // Observe actual replay bytes, not a sleep or scheduler assumption.
        let mut first = [0];
        client.read_exact(&mut first).await.unwrap();
        assert_eq!(first[0], b'{');
        assert!(matches!(
            replayed_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(!task.is_finished());
        emit_line(&id, retry, ts);
        gremlins_magic_bus::emit_event(EventKind::RunStopped { id: id.clone() });
        emit_line(&id, "after-terminal\n\n", ts);
        // Terminal and post-terminal events are already queued before follow
        // can begin. Keep the client's write side open so EOF cannot end it.
        let mut bytes = vec![first[0]];
        client.read_to_end(&mut bytes).await.unwrap();
        let offset = task.await.unwrap();
        replayed_rx.await.unwrap();
        let expected = format!(
            "{expected_history}{}{}",
            stamped(retry, ts),
            stamped("after-terminal\n\n", ts)
        );
        let frames: Vec<Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let expected_lines: Vec<_> = expected.lines().collect();
        assert_eq!(offset, expected_lines.len() as u64);
        assert_eq!(frames.len(), expected_lines.len());
        for (index, (frame, line)) in frames.iter().zip(expected_lines).enumerate() {
            assert_eq!(frame["type"], "log_line");
            assert_eq!(frame["offset"], index as u64);
            assert_eq!(frame["line"].as_str().unwrap(), line);
        }
    })
    .await
    .expect("replay/follow test timed out");
}
