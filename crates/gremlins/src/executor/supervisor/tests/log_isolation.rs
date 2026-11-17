use std::io::Write;
use std::path::Path;
use std::time::Duration;

use gremlins_magic_bus::{emit_event, EventKind};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::{create_chat_gremlin, create_session, model, spawn_test_actor, teardown_session};
use crate::executor::socket;
use crate::executor::state::{token_hex, BlobMode};
use crate::executor::supervisor::{actor::ActorMsg, actor_client::Client, handle_connection};
use crate::test_support::Sandbox;

struct ControlSocket {
    stream: BufReader<UnixStream>,
    server: JoinHandle<()>,
}

impl ControlSocket {
    async fn connect(actor: &UnboundedSender<ActorMsg>) -> Self {
        let (client, server) = UnixStream::pair().unwrap();
        let mut socket = Self {
            stream: BufReader::new(client),
            server: tokio::spawn(handle_connection(server, actor.clone())),
        };
        socket.send(json!({"op": "version"})).await;
        assert_eq!(
            socket.read().await,
            Some(json!({"type": "ok", "version": crate::VERSION}))
        );
        socket
    }

    async fn send(&mut self, request: Value) {
        socket::write_json_line(self.stream.get_mut(), &request)
            .await
            .unwrap();
    }

    async fn read(&mut self) -> Option<Value> {
        timeout(
            Duration::from_secs(5),
            socket::read_json_line(&mut self.stream),
        )
        .await
        .expect("control socket timed out")
        .unwrap()
    }

    async fn expect_line(&mut self, offset: u64, marker: &str) {
        let frame = self.read().await.expect("unexpected EOF in log stream");
        assert_eq!(frame["type"], "log_line", "{frame}");
        assert_eq!(frame["offset"], offset, "{frame}");
        assert!(frame.get("id").is_none(), "raw broadcast: {frame}");
        let line = frame["line"].as_str().unwrap();
        let body = line.split_once("] ").map_or(line, |(_, body)| body);
        assert_eq!(body, marker);
    }

    async fn expect_broadcast(&mut self, id: &str, marker: &str) {
        let expected = json!({"type": "log_line", "id": id, "line": marker});
        // This is a global listener; other concurrently running tests can publish too.
        timeout(Duration::from_secs(5), async {
            while self.read().await.expect("broadcast listener closed") != expected {}
        })
        .await
        .expect("missing log broadcast");
    }

    async fn close(self) {
        drop(self.stream);
        self.server.await.unwrap();
    }
}

async fn register_run(
    actor: &UnboundedSender<ActorMsg>,
    session: &str,
    project: &Path,
    name: &str,
) -> String {
    let id = format!("{name}-{}", token_hex(8));
    let gremlin = create_chat_gremlin(
        &id,
        &model("cmd/echo"),
        "helpful",
        project,
        &Default::default(),
    )
    .unwrap();
    let mut log = gremlin.state.open_blob("log", BlobMode::Write).unwrap();
    writeln!(log, "{name}-history").unwrap();
    drop(log);
    Client::new(actor.clone())
        .register_chat_gremlin(session, gremlin)
        .await;
    id
}

fn log_line(id: &str, marker: &str) {
    emit_event(EventKind::LogLine {
        id: id.into(),
        line: marker.into(),
    });
}

fn finish_run(actor: &UnboundedSender<ActorMsg>, id: &str) {
    actor
        .send(ActorMsg::RunFinished {
            id: id.into(),
            result: 0,
            notify_shutdown: false,
        })
        .unwrap();
}

async fn check_snapshot(actor: &UnboundedSender<ActorMsg>, a: &str, b: &str) {
    let mut snapshot = ControlSocket::connect(actor).await;
    log_line(b, "b-queued-before-snapshot");
    snapshot
        .send(json!({"op": "log", "id": a, "follow": false}))
        .await;
    snapshot.expect_line(0, "a-history").await;
    assert_eq!(snapshot.read().await, None);
    snapshot.close().await;
}

#[tokio::test]
async fn control_log_isolated_from_global_broadcasts() {
    let sandbox = Sandbox::new();
    let (actor, _) = spawn_test_actor().await;
    let session = create_session(&actor).await;
    let a = register_run(&actor, &session, sandbox.path(), "a").await;
    let b = register_run(&actor, &session, sandbox.path(), "b").await;

    check_snapshot(&actor, &a, &b).await;

    let mut global = ControlSocket::connect(&actor).await;
    global.send(json!({"op": "status", "id": a})).await;
    assert_eq!(global.read().await.unwrap()["type"], "ok");
    let mut follow = ControlSocket::connect(&actor).await;
    log_line(&b, "b-queued-before-follow");
    follow
        .send(json!({"op": "log", "id": a, "follow": true}))
        .await;
    follow.expect_line(0, "a-history").await;

    log_line(&a, "a-live");
    log_line(&b, "b-live");
    global.expect_broadcast(&a, "a-live").await;
    global.expect_broadcast(&b, "b-live").await;
    follow.expect_line(1, "a-live").await;

    finish_run(&actor, &b);
    assert!(!super::contains_run(&actor, &b).await);
    log_line(&a, "a-after-b-finished");
    follow.expect_line(2, "a-after-b-finished").await;
    finish_run(&actor, &a);
    assert_eq!(follow.read().await, None);
    follow.close().await;
    global.close().await;
    teardown_session(&actor, &session).await;
}
