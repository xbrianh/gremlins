use super::*;
use crate::clients::{
    client::ModelURI,
    execution_input::{ExecutionIdentity, InputReceiver, InputRegistry, InputRevoker},
};
use crate::executor::supervisor::{chat, CancelToken};
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test(start_paused = true)]
async fn pending_ack_times_out_releases_claim_and_ignores_buffered_bail() {
    let fixture = Fixture::new(false).await;
    let (mut inbox, _revoker) = fixture.inbox();
    let request = fixture.request();
    let bound = bind(&request, &fixture.registry, None).await.unwrap();
    let (mut peer, reader) = tokio::io::duplex(1024);
    peer.write_all(b"{\"cmd\":\"bail\",\"req_id\":\"buffered\"}\n")
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task_request = request.clone();
    let task = tokio::spawn(async move {
        let started = Arc::new(std::sync::Mutex::new(Some(started)));
        serve_bound_io(&task_request, bound, BufReader::new(reader), move |_| {
            if let Some(started) = started.lock().unwrap().take() {
                let _ = started.send(());
            }
            std::future::pending::<Result<(), String>>()
        })
        .await;
    });
    ready.await.unwrap();
    tokio::time::advance(super::super::input_flow::WRITE_TIMEOUT).await;
    task.await.unwrap();
    assert_eq!(start.elapsed(), super::super::input_flow::WRITE_TIMEOUT);
    assert!(matches!(
        inbox.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    // The peer and execution remain live: only the failed ACK released control.
    let rebound = bind(&request, &fixture.registry, None).await.unwrap();
    drop(rebound);
    drop(peer);
}

struct ReceiptRaceReader {
    command: Option<&'static [u8]>,
    settle_and_close: Option<Box<dyn FnOnce() + Send>>,
}

impl tokio::io::AsyncRead for ReceiptRaceReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(command) = self.command.take() {
            buf.put_slice(command);
            return std::task::Poll::Ready(Ok(()));
        }
        // select has already polled pending receipts and closed(), both Pending.
        // Deliver now, then wake closed() while the reader remains Pending.
        if let Some(settle_and_close) = self.settle_and_close.take() {
            settle_and_close();
        }
        std::task::Poll::Pending
    }
}

#[tokio::test]
async fn confirmation_delivered_during_closed_selection_stays_delivered() {
    use crate::clients::execution_input::InputCommand;

    let fixture = Fixture::new(false).await;
    let (mut inbox, revoker) = fixture.inbox();
    let request = fixture.request();
    let bound = bind(&request, &fixture.registry, None).await.unwrap();
    let reader = ReceiptRaceReader {
        command: Some(b"{\"cmd\":\"message\",\"text\":\"fix\",\"req_id\":\"race\"}\n"),
        settle_and_close: Some(Box::new(move || {
            let InputCommand::Correction(correction) = inbox.try_recv().unwrap() else {
                panic!("expected correction");
            };
            assert!(correction.incorporate(|_, _| {}));
            revoker.revoke();
        })),
    };
    let (output, mut frames) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(1),
        serve_bound_io(&request, bound, BufReader::new(reader), move |frame| {
            let output = output.clone();
            async move { output.send(frame).map_err(|e| e.to_string()) }
        }),
    )
    .await
    .unwrap();
    let mut actual = Vec::new();
    while let Some(frame) = frames.recv().await {
        actual.push(frame);
    }
    assert_eq!(actual.len(), 4, "{actual:?}");
    assert_eq!(actual[0]["type"], "input_bound");
    assert_eq!(actual[1], json!({"type":"queued", "req_id":"race"}));
    assert_eq!(actual[2], json!({"type":"delivered", "req_id":"race"}));
    assert_eq!(actual[3], json!({"type":"ended"}));
}

struct Fixture {
    registry: Arc<RunRegistry>,
    inputs: InputRegistry,
    _gremlin: crate::executor::gremlin::Gremlin,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new(chat: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gremlin = chat::create_chat_gremlin(
            "adapter",
            &ModelURI::parse("cmd/echo").unwrap(),
            "helpful",
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        gremlin.state.patch(
            &[],
            json!({"kind": if chat {"chat"} else {"pipeline"}})
                .as_object()
                .unwrap(),
        );
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let registry = Arc::new(RunRegistry::new(shutdown));
        let gremlin = registry
            .register_chat_gremlin("adapter".into(), gremlin, CancelToken::new())
            .await;
        let inputs = InputRegistry::default();
        registry
            .get_run_interactive("adapter")
            .await
            .unwrap()
            .0
            .set_input_registry(inputs.clone());
        Self {
            registry,
            inputs,
            _gremlin: gremlin,
            _dir: dir,
        }
    }
    fn inbox(&self) -> (InputReceiver, Arc<InputRevoker>) {
        self.inputs.register_with_nonce(
            ExecutionIdentity {
                run_id: "adapter".into(),
                stage_name: "build".into(),
                stage_run_id: "execution".into(),
            },
            Some(7),
            |_| {},
        )
    }
    fn request(&self) -> Value {
        json!({"op":"input", "id":"adapter", "stage":"build", "stage_run_id":"execution", "req_id":"bind"})
    }
}

#[test]
fn bound_enqueue_error_always_has_request_id() {
    assert_eq!(
        bound_error(&json!({"op":"chat"}), "full"),
        json!({"type":"error", "message":"full", "req_id":null})
    );
}

#[tokio::test]
async fn full_inbox_bound_error_ignores_spoofed_chat_operation() {
    let fixture = Fixture::new(false).await;
    let (_inbox, _revoker) = fixture.inbox();
    let request = fixture.request();
    let bound = bind(&request, &fixture.registry, None).await.unwrap();
    let mut held = Vec::new();
    while let Ok(receipt) = bound.sender.try_send("fill".into(), "fill".into()) {
        held.push(receipt);
    }
    let (mut peer, reader) = tokio::io::duplex(1024);
    let (output, mut frames) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        serve_bound_io(&request, bound, BufReader::new(reader), move |frame| {
            let output = output.clone();
            async move { output.send(frame).map_err(|e| e.to_string()) }
        })
        .await;
    });
    assert_eq!(frames.recv().await.unwrap()["type"], "input_bound");
    peer.write_all(b"{\"cmd\":\"message\",\"text\":\"fix\",\"op\":\"chat\",\"req_id\":\"full\"}\n")
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame["type"], "error");
    assert_eq!(frame["req_id"], "full");
    assert_eq!(
        frame["message"],
        crate::clients::execution_input::InputRejected::Full.to_string()
    );
    drop(peer);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}
