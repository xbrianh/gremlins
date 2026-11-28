//! Socket smoke tests; validation and receipt state tables live in input_rules.
use super::{create_chat_gremlin, create_session, model, spawn_test_actor, TestRegistry};
use crate::clients::execution_input::{
    ExecutionIdentity, InputCommand, InputReceiver, InputRegistry, InputRevoker,
};
use crate::executor::{
    socket,
    supervisor::{handle_connection, session, session_task::SessionTaskHandle, CancelToken},
};
use gremlins_magic_bus::connection::SharedWriter;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::UnixStream,
    task::JoinHandle,
    time::timeout,
};

const LIMIT: Duration = Duration::from_secs(5);

struct Peer {
    io: BufReader<UnixStream>,
    task: JoinHandle<()>,
    session: Option<String>,
}
impl Peer {
    async fn global(registry: &TestRegistry, request: Value) -> Self {
        let (server, client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(handle_connection(
            server,
            registry.clone(),
            Default::default(),
        ));
        let mut peer = Self {
            io: BufReader::new(client),
            task,
            session: None,
        };
        peer.send(request).await;
        peer
    }
    async fn session(registry: &TestRegistry) -> (Self, SessionTaskHandle) {
        let (id, owner) = create_session(registry).await;
        let (server, client) = UnixStream::pair().unwrap();
        let (read, write) = server.into_split();
        let task = tokio::spawn(session::handle_session_main(
            read,
            SharedWriter::new(write),
            id.clone(),
            registry.clone(),
            owner.clone(),
        ));
        (
            Self {
                io: BufReader::new(client),
                task,
                session: Some(id),
            },
            owner,
        )
    }
    async fn send(&mut self, mut request: Value) {
        if let Some(id) = &self.session {
            request["session_id"] = json!(id);
        }
        timeout(LIMIT, socket::write_json_line(self.io.get_mut(), &request))
            .await
            .unwrap()
            .unwrap();
    }
    async fn frame(&mut self) -> Value {
        timeout(LIMIT, socket::read_json_line(&mut self.io))
            .await
            .unwrap()
            .unwrap()
            .expect("unexpected EOF")
    }
    async fn expect(&mut self, kind: &str, req: &str) {
        let frame = self.frame().await;
        assert_eq!(frame["type"], kind, "{frame}");
        assert_eq!(frame["req_id"], req, "{frame}");
    }
    async fn disconnect(&mut self) {
        self.io.get_mut().shutdown().await.unwrap();
        timeout(LIMIT, &mut self.task).await.unwrap().unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    registry: TestRegistry,
    inputs: InputRegistry,
    _gremlin: crate::executor::gremlin::Gremlin,
    _dir: tempfile::TempDir,
}
impl Fixture {
    async fn new(chat: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gremlin = create_chat_gremlin(
            "input-wire",
            &model("cmd/echo"),
            "helpful",
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        gremlin.state.patch(
            &[],
            json!({"kind":if chat {"chat"} else {"pipeline"}})
                .as_object()
                .unwrap(),
        );
        let (registry, _) = spawn_test_actor().await;
        let gremlin = registry
            .register_chat_gremlin("input-wire".into(), gremlin, CancelToken::new())
            .await;
        let inputs = InputRegistry::default();
        let (interactive, _, _) = registry.get_run_interactive("input-wire").await.unwrap();
        interactive.set_input_registry(inputs.clone());
        Self {
            registry,
            inputs,
            _gremlin: gremlin,
            _dir: dir,
        }
    }
    fn inbox(&self, execution: &str) -> (InputReceiver, Arc<InputRevoker>) {
        self.inputs
            .register_with_nonce(identity(execution), Some(7), |_| {})
    }
    async fn stop(&self) {
        self.registry.stop_run("input-wire").await.unwrap();
    }
}
fn identity(execution: &str) -> ExecutionIdentity {
    ExecutionIdentity {
        run_id: "input-wire".into(),
        stage_name: "build".into(),
        stage_run_id: execution.into(),
    }
}
fn request(execution: &str) -> Value {
    json!({"op":"input", "id":"input-wire", "stage":"build", "stage_run_id":execution, "req_id":"bind"})
}
async fn bound(peer: &mut Peer, execution: &str) {
    assert_eq!(
        peer.frame().await,
        json!({"type":"input_bound", "id":"input-wire", "stage":"build", "stage_run_id":execution, "req_id":"bind"})
    );
}
async fn receive(receiver: &mut InputReceiver) -> InputCommand {
    timeout(LIMIT, receiver.recv()).await.unwrap().unwrap()
}
fn incorporate(command: InputCommand) {
    let InputCommand::Correction(correction) = command else {
        panic!("expected correction")
    };
    assert!(correction.incorporate(|id, text| {
        assert_eq!(id, "revision");
        assert_eq!(text, "fix");
    }));
}
async fn queue(peer: &mut Peer) {
    peer.send(json!({"cmd":"message", "text":"fix", "req_id":"revision"}))
        .await;
    peer.expect("queued", "revision").await;
}

#[tokio::test]
async fn a_global_bound_queued_delivered() {
    let f = Fixture::new(false).await;
    let (mut inbox, _revoke) = f.inbox("live");
    let mut peer = Peer::global(&f.registry, request("live")).await;
    bound(&mut peer, "live").await;
    queue(&mut peer).await;
    incorporate(receive(&mut inbox).await);
    peer.expect("delivered", "revision").await;
    peer.disconnect().await;
    f.stop().await;
}

#[tokio::test]
async fn b_owned_session_targeted_chat_queued_delivered() {
    let f = Fixture::new(true).await;
    let (mut inbox, _revoke) = f.inbox("live");
    let (mut peer, owner) = Peer::session(&f.registry).await;
    owner.add_chat_id("input-wire").await;
    peer.send(json!({"op":"chat", "text":"fix", "req_id":"revision", "target":{"id":"input-wire", "nonce":7, "stage":"build", "stage_run_id":"live"}})).await;
    let ack = peer.frame().await;
    assert_eq!(ack["type"], "ok", "{ack}");
    assert_eq!(ack["status"], "queued", "{ack}");
    assert_eq!(ack["req_id"], "revision");
    incorporate(receive(&mut inbox).await);
    peer.expect("delivered", "revision").await;
    peer.disconnect().await;
}

#[tokio::test]
async fn c_competing_claim_rebind_after_disconnect() {
    let f = Fixture::new(false).await;
    let (inbox, _revoke) = f.inbox("live");
    let mut owner = Peer::global(&f.registry, request("live")).await;
    bound(&mut owner, "live").await;
    let mut contender = Peer::global(&f.registry, request("live")).await;
    assert_eq!(
        contender.frame().await,
        json!({"type":"error", "message":"controller_busy", "req_id":"bind"})
    );
    owner.disconnect().await;
    let mut replacement = Peer::global(&f.registry, request("live")).await;
    bound(&mut replacement, "live").await;
    assert!(!inbox.pending());
    replacement.disconnect().await;
    f.stop().await;
}

#[tokio::test]
async fn d_held_receipt_disconnect_synthesizes_no_command() {
    let f = Fixture::new(false).await;
    let (mut inbox, _revoke) = f.inbox("live");
    let mut peer = Peer::global(&f.registry, request("live")).await;
    bound(&mut peer, "live").await;
    queue(&mut peer).await;
    let held = receive(&mut inbox).await;
    peer.disconnect().await;
    let mut replacement = Peer::global(&f.registry, request("live")).await;
    bound(&mut replacement, "live").await;
    assert!(inbox.try_recv().is_err());
    assert!(!inbox.pending());
    drop(held);
    replacement.disconnect().await;
    f.stop().await;
}

#[tokio::test]
async fn e_revoke_held_dropped_ended_successor_has_no_input() {
    let f = Fixture::new(false).await;
    let (mut inbox, revoke) = f.inbox("old");
    let mut peer = Peer::global(&f.registry, request("old")).await;
    bound(&mut peer, "old").await;
    queue(&mut peer).await;
    let held = receive(&mut inbox).await;
    revoke.revoke();
    let (mut successor, _next_revoke) = f.inbox("successor");
    peer.expect("dropped", "revision").await;
    assert_eq!(peer.frame().await, json!({"type":"ended"}));
    let InputCommand::Correction(correction) = held else {
        panic!("expected correction")
    };
    assert!(!correction.incorporate(|_, _| panic!("revoked input applied")));
    assert!(successor.try_recv().is_err());
    assert!(!successor.pending());
    let mut next = Peer::global(&f.registry, request("successor")).await;
    bound(&mut next, "successor").await;
    next.disconnect().await;
    f.stop().await;
}
