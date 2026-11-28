use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use gremlins_magic_bus::EventKind;
use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{stream::Opened, DaemonSession};
use crate::tui::client::{ChatEvent, DaemonClient, FakeReply, FakeRequest};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("test timed out")
}

struct Opening {
    session: String,
    run: String,
    replay: bool,
    reply: oneshot::Sender<Result<Opened, String>>,
}

struct Reader {
    tx: mpsc::Sender<Value>,
    alive: Arc<AtomicBool>,
}

struct Alive(Arc<AtomicBool>);
impl Drop for Alive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Opening {
    fn accept(self, run: &str, replay: bool) -> Reader {
        self.accept_buffered(run, replay, vec![])
    }

    fn accept_buffered(self, run: &str, replay: bool, frames: Vec<Value>) -> Reader {
        assert_eq!(self.session, "owner");
        assert_eq!(self.run, run);
        assert_eq!(self.replay, replay);
        let (tx, rx) = mpsc::channel(32);
        for frame in frames {
            tx.try_send(frame).unwrap();
        }
        let alive = Arc::new(AtomicBool::new(true));
        let guard = Alive(alive.clone());
        let reader = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        self.reply.send(Ok((rx, reader))).unwrap();
        Reader { tx, alive }
    }
}

struct Fixture {
    session: DaemonSession,
    requests: mpsc::UnboundedReceiver<FakeRequest>,
    pending: std::collections::VecDeque<FakeReply>,
    opens: mpsc::UnboundedReceiver<Opening>,
    chat: mpsc::UnboundedReceiver<ChatEvent>,
    raw: mpsc::UnboundedReceiver<Value>,
}

impl Fixture {
    fn new() -> Self {
        let (client, requests, events, chat, raw) = DaemonClient::fake_with_requests(
            json!({"type":"ok", "session_id":"owner", "chat_run_id":"reserved"}),
        );
        let mut session = DaemonSession::from_client(client, events, chat, raw);
        let chat = session.take_chat_rx();
        let raw = session.take_raw_rx();
        let (tx, opens) = mpsc::unbounded_channel();
        session
            .chat_watch
            .set_opener(Arc::new(move |session, run, replay| {
                let (reply, rx) = oneshot::channel();
                tx.send(Opening {
                    session,
                    run,
                    replay,
                    reply,
                })
                .unwrap();
                Box::pin(async move { rx.await.expect("opener response dropped") })
            }));
        Self {
            session,
            requests,
            pending: Default::default(),
            opens,
            chat,
            raw,
        }
    }

    fn prompt(&self, turn: &str) -> JoinHandle<Result<Value, String>> {
        let client = self.session.client().clone();
        let turn = turn.to_owned();
        tokio::spawn(async move {
            client
                .send_session_request(json!({"op":"chat", "message":"hello", "turn":turn}))
                .await
        })
    }

    async fn request(&mut self, turn: &str) {
        let FakeRequest { request, reply } = bounded(self.requests.recv()).await.unwrap();
        assert_eq!(request["op"], "chat");
        assert_eq!(request["session_id"], "owner");
        assert_eq!(request["turn"], turn);
        assert_eq!(request["message"], "hello");
        self.pending.push_back(reply);
    }

    async fn reply(&mut self, value: Value) {
        self.pending.pop_front().unwrap().send(Ok(value)).unwrap();
    }

    async fn ack(&mut self, run: &str) {
        self.reply(json!({"type":"ok", "enqueued":true, "session_id":"owner", "chat_run_id":run}))
            .await;
    }

    async fn chunk(&mut self, reader: &Reader, run: &str, turn: &str) {
        reader
            .tx
            .send(json!({"type":"stream_chunk", "text":"early", "run_id":run,
            "stage_type":"agent", "stage_run_id":"execution", "correlation_turn":turn,
            "turn":"lower-priority-turn", "chat_turn":"lowest-priority-turn"}))
            .await
            .unwrap();
        let (event, actual_turn, execution) = bounded(self.chat.recv()).await.unwrap();
        assert!(matches!(event, EventKind::StreamChunk { text } if text == "early"));
        assert_eq!(actual_turn.as_deref(), Some(turn));
        let execution = execution.unwrap();
        assert_eq!(execution.run_id, run);
        assert_eq!(execution.stage_run_id, "execution");
        assert_eq!(execution.nonce, 0);
    }
}

#[tokio::test]
async fn two_prompts_deliver_before_ack_and_open_exactly_one_watch() {
    let mut f = Fixture::new();
    let first = f.prompt("one");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    f.chunk(&reader, "reserved", "one").await;
    assert!(!first.is_finished());
    let second = f.prompt("two");
    f.request("two").await;
    f.chunk(&reader, "reserved", "two").await;
    assert!(!second.is_finished());
    assert!(!first.is_finished());
    f.ack("reserved").await;
    bounded(first).await.unwrap().unwrap();
    f.ack("reserved").await;
    bounded(second).await.unwrap().unwrap();
    assert!(f.chat.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn rejection_preserves_reservation_for_next_prompt() {
    let mut f = Fixture::new();
    let first = f.prompt("rejected");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("rejected").await;
    f.reply(json!({"type":"error", "message":"not accepted"}))
        .await;
    assert_eq!(bounded(first).await.unwrap().unwrap()["type"], "error");
    let second = f.prompt("accepted");
    f.request("accepted").await;
    f.chunk(&reader, "reserved", "accepted").await;
    f.ack("reserved").await;
    bounded(second).await.unwrap().unwrap();
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn replacement_cancels_reservation_before_opening_replay() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let old = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    f.chunk(&old, "reserved", "early-reserved").await;
    f.ack("replacement").await;
    let opening = bounded(f.opens.recv()).await.unwrap();
    assert!(
        !old.alive.load(Ordering::SeqCst),
        "old reader must stop before replacement opener"
    );
    assert!(old.tx.is_closed());
    let replacement = opening.accept_buffered(
        "replacement",
        true,
        vec![
            json!({"type":"stream_chunk", "text":"early-1", "run_id":"replacement", "turn":"one"}),
            json!({"type":"stream_chunk", "text":"early-2", "run_id":"replacement", "turn":"one"}),
        ],
    );
    bounded(prompt).await.unwrap().unwrap();
    for expected in ["early-1", "early-2"] {
        let (event, turn, execution) = bounded(f.chat.recv()).await.unwrap();
        assert!(matches!(event, EventKind::StreamChunk { text } if text == expected));
        assert_eq!(turn.as_deref(), Some("one"));
        assert!(execution.is_none());
    }
    f.chunk(&replacement, "replacement", "live").await;
    assert!(f.chat.try_recv().is_err());
    let next = f.prompt("two");
    f.request("two").await;
    f.ack("replacement").await;
    bounded(next).await.unwrap().unwrap();
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn failed_initial_reservation_still_enqueues_and_replays_confirmed_run() {
    failed_reservation("confirmed").await;
}

#[tokio::test]
async fn failed_initial_reservation_replays_even_when_ack_keeps_reserved_identity() {
    failed_reservation("reserved").await;
}

async fn failed_reservation(confirmed: &str) {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let opening = bounded(f.opens.recv()).await.unwrap();
    assert!(!opening.replay);
    opening
        .reply
        .send(Err("reservation unavailable".into()))
        .unwrap();
    f.request("one").await;
    f.ack(confirmed).await;
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept(confirmed, true);
    bounded(prompt).await.unwrap().unwrap();
    f.chunk(&reader, confirmed, "one").await;
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn recreate_session_and_close_cancel_without_background_reopen() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    f.ack("reserved").await;
    bounded(prompt).await.unwrap().unwrap();
    let client = f.session.client().clone();
    let recreate = tokio::spawn(async move { client.recreate_session().await });
    let close = bounded(f.requests.recv()).await.unwrap();
    assert_eq!(
        close.request,
        json!({"op":"close_session", "session_id":"owner"})
    );
    bounded(reader.tx.closed()).await;
    assert!(!reader.alive.load(Ordering::SeqCst));
    close.reply.send(Ok(json!({"type":"ok"}))).unwrap();
    let create = bounded(f.requests.recv()).await.unwrap();
    assert_eq!(create.request, json!({"op":"create_session"}));
    create
        .reply
        .send(Ok(
            json!({"session_id":"owner", "chat_run_id":"new-reserved"}),
        ))
        .unwrap();
    bounded(recreate).await.unwrap().unwrap();
    assert_eq!(f.session.client().session_generation().await, 1);
    assert_eq!(f.session.client().chat_run_id().await, "new-reserved");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(f.opens.try_recv().is_err());
    let next = f.prompt("two");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("new-reserved", false);
    f.request("two").await;
    f.chunk(&reader, "new-reserved", "two").await;
    f.ack("new-reserved").await;
    bounded(next).await.unwrap().unwrap();
    f.session.close().await;
    bounded(reader.tx.closed()).await;
    f.session.chat_watch.reset().await;
    assert!(f
        .session
        .chat_watch
        .ensure(f.session.client(), 0, "owner", "reserved")
        .await
        .is_err());
    assert!(reader
        .tx
        .send(json!({"type":"stream_chunk", "text":"after close", "run_id":"new-reserved"}))
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(f.chat.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
}

async fn no_more_chat(f: &mut Fixture) {
    assert!(
        tokio::time::timeout(Duration::from_millis(150), f.chat.recv())
            .await
            .is_err()
    );
}

fn frame(run: &str, sequence: u64, text: &str) -> Value {
    json!({"type":"stream_chunk", "run_id":run, "sequence":sequence,
        "turn":"one", "text":text})
}

#[tokio::test]
async fn mid_turn_disconnect_replays_output_without_resubmitting_prompt() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    reader
        .tx
        .send(frame("reserved", 1, "before"))
        .await
        .unwrap();
    let (event, turn, _) = bounded(f.chat.recv()).await.unwrap();
    assert!(matches!(event, EventKind::StreamChunk { text } if text == "before"));
    assert_eq!(turn.as_deref(), Some("one"));
    assert!(!prompt.is_finished());
    drop(reader.tx);
    let replacement = bounded(f.opens.recv()).await.unwrap().accept_buffered(
        "reserved",
        true,
        vec![
            frame("reserved", 1, "before"),
            frame("reserved", 2, "after"),
        ],
    );
    let (event, turn, _) = bounded(f.chat.recv()).await.unwrap();
    assert!(matches!(event, EventKind::StreamChunk { text } if text == "after"));
    assert_eq!(turn.as_deref(), Some("one"));
    f.ack("reserved").await;
    bounded(prompt).await.unwrap().unwrap();
    replacement
        .tx
        .send(frame("reserved", 2, "after"))
        .await
        .unwrap();
    f.chunk(&replacement, "reserved", "live").await;
    no_more_chat(&mut f).await;
    assert!(
        f.requests.try_recv().is_err(),
        "recovery must not enqueue or input"
    );
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn malformed_enqueue_ack_does_not_publish_identity() {
    for ack in [
        json!({"type":"ok", "chat_run_id":"bad"}),
        json!({"type":"ok", "enqueued":false, "chat_run_id":"bad"}),
        json!({"type":"ok", "enqueued":true}),
        json!({"type":"ok", "enqueued":true, "chat_run_id":""}),
        json!({"type":"ok", "enqueued":true, "chat_run_id":42}),
        json!({"type":"ok", "enqueued":true, "session_id":"other", "chat_run_id":"bad"}),
    ] {
        let mut f = Fixture::new();
        let prompt = f.prompt("one");
        let reader = bounded(f.opens.recv())
            .await
            .unwrap()
            .accept("reserved", false);
        f.request("one").await;
        f.reply(ack).await;
        assert!(bounded(prompt).await.unwrap().is_err());
        assert_eq!(f.session.client().chat_run_id().await, "reserved");
        assert!(f.raw.try_recv().is_err());
        assert!(f.opens.try_recv().is_err());
        f.chunk(&reader, "reserved", "still-reserved").await;
        f.session.close().await;
    }
}

#[tokio::test]
async fn retired_session_in_flight_ack_does_not_publish_identity() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let _reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    recreate(&mut f).await;
    f.ack("stale-run").await;
    assert!(bounded(prompt).await.unwrap().is_err());
    assert_eq!(f.session.client().chat_run_id().await, "new-reserved");
    assert!(f.raw.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

async fn recreate(f: &mut Fixture) {
    let client = f.session.client().clone();
    let task = tokio::spawn(async move { client.recreate_session().await });
    let close = bounded(f.requests.recv()).await.unwrap();
    assert_eq!(
        close.request,
        json!({"op":"close_session", "session_id":"owner"})
    );
    close.reply.send(Ok(json!({"type":"ok"}))).unwrap();
    let create = bounded(f.requests.recv()).await.unwrap();
    assert_eq!(create.request, json!({"op":"create_session"}));
    create
        .reply
        .send(Ok(
            json!({"session_id":"owner", "chat_run_id":"new-reserved"}),
        ))
        .unwrap();
    bounded(task).await.unwrap().unwrap();
}

fn emit(reader: &Reader, run: &str) -> JoinHandle<()> {
    let tx = reader.tx.clone();
    let run = run.to_owned();
    tokio::spawn(async move {
        loop {
            if tx
                .send(json!({"type":"stream_chunk", "run_id":run,
                "turn":"one", "text":"retiring"}))
                .await
                .is_err()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
}

#[tokio::test]
async fn recreation_and_close_retire_actively_emitting_readers() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let old = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    f.ack("reserved").await;
    bounded(prompt).await.unwrap().unwrap();
    let emitter = emit(&old, "reserved");
    bounded(f.chat.recv()).await.unwrap();
    recreate(&mut f).await;
    // Drain synchronously at the return boundary, before yielding again.
    while f.chat.try_recv().is_ok() {}
    bounded(emitter).await.unwrap();
    assert!(old.tx.is_closed());
    no_more_chat(&mut f).await;
    let prompt = f.prompt("two");
    let replacement = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("new-reserved", false);
    f.request("two").await;
    f.ack("new-reserved").await;
    bounded(prompt).await.unwrap().unwrap();
    for _ in 0..8 {
        assert!(old.tx.send(frame("reserved", 1, "stale")).await.is_err());
        f.chunk(&replacement, "new-reserved", "replacement-live")
            .await;
    }
    let emitter = emit(&replacement, "new-reserved");
    bounded(f.chat.recv()).await.unwrap();
    f.session.close().await;
    while f.chat.try_recv().is_ok() {}
    bounded(emitter).await.unwrap();
    no_more_chat(&mut f).await;
    assert!(replacement.tx.is_closed());
    assert!(f.opens.try_recv().is_err());
}

#[tokio::test]
async fn overlapping_prompt_acquisition_cannot_steal_replacement_replay() {
    overlapping_acquisition(false).await;
}

#[tokio::test]
async fn overlapping_prompt_acquisition_after_failed_reservation_keeps_replay() {
    overlapping_acquisition(true).await;
}

async fn overlapping_acquisition(fail: bool) {
    let mut f = Fixture::new();
    let first = f.prompt("one");
    let opening = bounded(f.opens.recv()).await.unwrap();
    if fail {
        opening
            .reply
            .send(Err("reservation failed".into()))
            .unwrap();
    } else {
        opening.accept("reserved", false);
    }
    f.request("one").await;
    let contender = if fail {
        let contender = f.prompt("contender");
        let reservation = bounded(f.opens.recv()).await.unwrap();
        assert_eq!(reservation.run, "reserved");
        assert!(
            reservation.replay,
            "failed reservation requires replay even for a concurrent prompt"
        );
        f.ack("replacement").await;
        // Publication must wait for the watch acquisition already in flight.
        tokio::task::yield_now().await;
        assert_eq!(f.session.client().chat_run_id().await, "reserved");
        reservation.accept("reserved", true);
        f.request("contender").await;
        Some(contender)
    } else {
        f.ack("replacement").await;
        None
    };
    // Hold confirmation's opener (and watch lock) while another submission
    // attempts acquisition. It must reuse the replay watch, never replace it.
    let opening = bounded(f.opens.recv()).await.unwrap();
    let second = f.prompt("two");
    tokio::task::yield_now().await;
    assert!(!second.is_finished());
    let reader = opening.accept_buffered(
        "replacement",
        true,
        vec![frame("replacement", 1, "replayed")],
    );
    bounded(first).await.unwrap().unwrap();
    if let Some(contender) = contender {
        f.ack("replacement").await;
        bounded(contender).await.unwrap().unwrap();
    }
    f.request("two").await;
    f.ack("replacement").await;
    bounded(second).await.unwrap().unwrap();
    let (event, _, _) = bounded(f.chat.recv()).await.unwrap();
    assert!(matches!(event, EventKind::StreamChunk { text } if text == "replayed"));
    f.chunk(&reader, "replacement", "two").await;
    assert!(f.opens.try_recv().is_err());
    assert!(f.requests.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn wire_conversion_preserves_turn_precedence_and_only_agent_execution() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    f.ack("reserved").await;
    bounded(prompt).await.unwrap().unwrap();
    for (stage_type, stage_run, turn, chat_turn, expected_turn, has_execution) in [
        (
            "agent",
            "execution",
            json!(7),
            json!("fallback"),
            "fallback",
            true,
        ),
        (
            "agent",
            "execution",
            json!("wire-turn"),
            json!("fallback"),
            "wire-turn",
            true,
        ),
        (
            "exec",
            "execution",
            json!("wire-turn"),
            Value::Null,
            "wire-turn",
            false,
        ),
        (
            "agent",
            " ",
            json!("wire-turn"),
            Value::Null,
            "wire-turn",
            false,
        ),
    ] {
        reader
            .tx
            .send(
                json!({"type":"stream_chunk", "text":"wire", "run_id":"reserved",
            "stage_type":stage_type, "stage_run_id":stage_run, "nonce":987,
            "turn":turn, "chat_turn":chat_turn}),
            )
            .await
            .unwrap();
        let (event, turn, execution) = bounded(f.chat.recv()).await.unwrap();
        assert!(matches!(event, EventKind::StreamChunk { text } if text == "wire"));
        assert_eq!(turn.as_deref(), Some(expected_turn));
        assert_eq!(execution.is_some(), has_execution);
        if let Some(execution) = execution {
            assert_eq!(execution.run_id, "reserved");
            assert_eq!(execution.stage_run_id, "execution");
            assert_eq!(execution.nonce, 0);
        }
    }
    f.chunk(&reader, "reserved", "correlation-wins").await;
    for kind in ["lagged", "history_gap"] {
        reader.tx.send(json!({"type":kind})).await.unwrap();
        let (event, turn, execution) = bounded(f.chat.recv()).await.unwrap();
        assert!(
            matches!(event, EventKind::SystemWarning { turn: None, message }
            if message.contains("history discontinuity"))
        );
        assert!(turn.is_none());
        assert!(execution.is_none());
    }
    f.session.close().await;
}

#[tokio::test]
async fn every_session_request_is_rejected_during_recreate_and_after_shutdown() {
    let mut f = Fixture::new();
    let client = f.session.client().clone();
    let recreating = tokio::spawn(async move { client.recreate_session().await });
    let close = bounded(f.requests.recv()).await.unwrap();
    assert_eq!(close.request["op"], "close_session");
    for request in [
        json!({"op":"status"}),
        json!({"op":"chat", "target":"execution", "message":"hello"}),
        json!({"op":"chat", "message":"hello"}),
    ] {
        assert!(
            bounded(f.session.client().send_session_request(request.clone()))
                .await
                .is_err()
        );
        assert!(bounded(
            f.session
                .client()
                .send_session_request_for_generation(request, 0)
        )
        .await
        .is_err());
    }
    assert!(f.requests.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
    close.reply.send(Ok(json!({"type":"ok"}))).unwrap();
    let create = bounded(f.requests.recv()).await.unwrap();
    assert_eq!(create.request["op"], "create_session");
    create
        .reply
        .send(Ok(json!({"session_id":"owner", "chat_run_id":"fresh"})))
        .unwrap();
    bounded(recreating).await.unwrap().unwrap();
    f.session.close().await;
    for request in [
        json!({"op":"status"}),
        json!({"op":"chat", "target":"execution", "message":"hello"}),
        json!({"op":"chat", "message":"hello"}),
    ] {
        assert!(bounded(f.session.client().send_session_request(request))
            .await
            .is_err());
    }
    assert!(f.requests.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
}

#[tokio::test]
async fn same_run_ack_after_terminal_does_not_replay_until_next_prompt() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("one").await;
    reader
        .tx
        .send(json!({"type":"run_completed", "id":"reserved", "run_id":"reserved"}))
        .await
        .unwrap();
    bounded(reader.tx.closed()).await;
    f.ack("reserved").await;
    bounded(prompt).await.unwrap().unwrap();
    assert!(f.opens.try_recv().is_err());
    let next = f.prompt("two");
    let _reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("two").await;
    f.ack("reserved").await;
    bounded(next).await.unwrap().unwrap();
    f.session.close().await;
}

#[tokio::test]
async fn pending_reservation_is_interrupted_by_recreate_before_ack() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let mut opening = bounded(f.opens.recv()).await.unwrap();
    // Keep the handshake unanswered. Lifecycle retirement must cancel it,
    // rather than waiting for its network timeout or allowing enqueue.
    recreate(&mut f).await;
    bounded(opening.reply.closed()).await;
    assert!(bounded(prompt).await.unwrap().is_err());
    assert!(f.requests.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
    f.session.close().await;
}

#[tokio::test]
async fn pending_reservation_is_interrupted_by_shutdown_before_ack() {
    let mut f = Fixture::new();
    let prompt = f.prompt("one");
    let mut opening = bounded(f.opens.recv()).await.unwrap();
    bounded(f.session.close()).await;
    bounded(opening.reply.closed()).await;
    assert!(bounded(prompt).await.unwrap().is_err());
    assert!(f.requests.try_recv().is_err());
    assert!(f.opens.try_recv().is_err());
}

#[tokio::test]
async fn delayed_confirmation_cannot_replace_newer_delivered_identity() {
    let mut f = Fixture::new();
    let first = f.prompt("older");
    let _reader = bounded(f.opens.recv())
        .await
        .unwrap()
        .accept("reserved", false);
    f.request("older").await;
    let second = f.prompt("newer");
    f.request("newer").await;
    // Deliver both replies without yielding. Neither confirmation may publish
    // before the newer delivery has its authoritative order token.
    f.pending
        .pop_front()
        .unwrap()
        .send(Ok(json!({
            "type":"ok", "enqueued":true, "session_id":"owner", "chat_run_id":"older-run"
        })))
        .unwrap();
    f.pending
        .pop_front()
        .unwrap()
        .send(Ok(json!({
            "type":"ok", "enqueued":true, "session_id":"owner", "chat_run_id":"newer-run"
        })))
        .unwrap();
    let openings = tokio::spawn(async move {
        while let Some(opening) = f.opens.recv().await {
            let run = opening.run.clone();
            opening.accept(&run, true);
        }
    });
    // A superseded confirmation may return a stale-identity error; it must
    // never regress the identity or leave the newer request blocked.
    let _ = bounded(first).await.unwrap();
    bounded(second).await.unwrap().unwrap();
    assert_eq!(f.session.client().chat_run_id().await, "newer-run");
    let identities: Vec<_> = std::iter::from_fn(|| f.raw.try_recv().ok())
        .filter(|value| value["type"] == "session_chat_run_identity")
        .collect();
    assert_eq!(identities.last().unwrap()["id"], "newer-run");
    f.session.close().await;
    openings.abort();
}
