//! Exact input wire contract; inbox identities never come from bus consumption.
use super::{create_chat_gremlin, create_session, model, spawn_test_actor, TestRegistry};
use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop, test_ctx};
use crate::clients::execution_input::{
    ExecutionIdentity, InputCommand, InputReceiver, InputRegistry, InputRevoker, INPUT_CAPACITY,
};
use crate::executor::socket;
use crate::executor::supervisor::{
    handle_connection, session, session_task::SessionTaskHandle, CancelToken,
};
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent as Event};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    task::JoinHandle,
    time::timeout,
};

const LIMIT: Duration = Duration::from_secs(5);

struct Peer {
    io: BufReader<UnixStream>,
    task: JoinHandle<()>,
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
        };
        peer.send(request).await;
        peer
    }
    async fn send(&mut self, request: Value) {
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
    async fn quiet(&mut self) {
        assert!(timeout(
            Duration::from_millis(30),
            socket::read_json_line(&mut self.io)
        )
        .await
        .is_err());
    }
    async fn detach(&mut self) {
        self.send(json!({"cmd":"detach"})).await;
        timeout(LIMIT, &mut self.task).await.unwrap().unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Use the real listener: its aux path supplies a trusted owner, unlike the
// deliberately unowned handle_session_aux entry point.
struct Session {
    id: String,
    owner: SessionTaskHandle,
    path: std::path::PathBuf,
    _main: BufReader<UnixStream>,
    task: JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Session {
    async fn new(registry: &TestRegistry) -> Self {
        let (id, owner) = create_session(registry).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(session::run_session_listener(
            listener,
            id.clone(),
            registry.clone(),
            owner.clone(),
        ));
        let main = UnixStream::connect(&path).await.unwrap();
        Self {
            id,
            owner,
            path,
            _main: BufReader::new(main),
            task,
            _dir: dir,
        }
    }
    async fn open(&self, mut request: Value) -> Peer {
        request["session_id"] = json!(self.id);
        let client = UnixStream::connect(&self.path).await.unwrap();
        let mut peer = Peer {
            io: BufReader::new(client),
            task: tokio::spawn(async {}),
        };
        peer.send(request).await;
        peer
    }
    async fn close(&self) {
        self.owner.teardown(true).await;
    }
}
impl Drop for Session {
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
            "exact-wire",
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
            .register_chat_gremlin("exact-wire".into(), gremlin, CancelToken::new())
            .await;
        let inputs = InputRegistry::default();
        let (interactive, _, _) = registry.get_run_interactive("exact-wire").await.unwrap();
        interactive.set_input_registry(inputs.clone());
        Self {
            registry,
            inputs,
            _gremlin: gremlin,
            _dir: dir,
        }
    }
    fn inbox(&self, stage: &str, execution: &str) -> (InputReceiver, Arc<InputRevoker>) {
        self.inputs.register(identity(stage, execution), |_| {})
    }
    async fn stop(&self) {
        self.registry.stop_run("exact-wire").await.unwrap();
    }
}
fn identity(stage: &str, execution: &str) -> ExecutionIdentity {
    ExecutionIdentity {
        run_id: "exact-wire".into(),
        stage_name: stage.into(),
        stage_run_id: execution.into(),
    }
}
fn request(stage: &str, execution: &str) -> Value {
    json!({"op":"input", "id":"exact-wire", "stage":stage, "stage_run_id":execution, "req_id":"bind"})
}
async fn bound(peer: &mut Peer, stage: &str, execution: &str) {
    let ack = peer.frame().await;
    assert_eq!(
        ack,
        json!({"type":"input_bound","id":"exact-wire","stage":stage,"stage_run_id":execution,"req_id":"bind"})
    );
}
async fn receive(receiver: &mut InputReceiver) -> InputCommand {
    timeout(LIMIT, receiver.recv()).await.unwrap().unwrap()
}
fn incorporate(command: InputCommand, req: &str) {
    match command {
        InputCommand::Correction(c) => {
            assert!(c.incorporate(|id, text| {
                assert_eq!(id, req);
                assert_eq!(text, "revision");
            }));
        }
        InputCommand::Control(c) => {
            assert!(c.incorporate(|id, _| assert_eq!(id, req)));
        }
    }
}

#[tokio::test]
async fn sibling_exact_lookup_routes_independently_of_snapshot_and_receipts() {
    let mut f = Fixture::new(false).await;
    let mut snapshot = f._gremlin.next_interactive_session();
    assert!(!snapshot.accept_pause());
    let (mut a, _ra) = f.inbox("left", "left-1");
    let (mut b, _rb) = f.inbox("right", "right-1");
    let session = Session::new(&f.registry).await;
    let mut left = Peer::global(&f.registry, request("left", "left-1")).await;
    let mut right = session.open(request("right", "right-1")).await;
    bound(&mut left, "left", "left-1").await;
    bound(&mut right, "right", "right-1").await;
    left.send(json!({"cmd":"message","text":"revision","req_id":"a"}))
        .await;
    right
        .send(json!({"cmd":"message","text":"revision","req_id":"b"}))
        .await;
    left.expect("queued", "a").await;
    right.expect("queued", "b").await;
    let ca = receive(&mut a).await;
    let cb = receive(&mut b).await;
    left.quiet().await;
    right.quiet().await;
    incorporate(cb, "b");
    right.expect("delivered", "b").await;
    drop(ca);
    left.expect("dropped", "a").await;
    for cmd in ["pause", "step", "resume", "bail"] {
        left.send(json!({"cmd":cmd,"req_id":cmd})).await;
        left.expect("queued", cmd).await;
        let InputCommand::Control(control) = receive(&mut a).await else {
            panic!("not a control")
        };
        assert!(control.incorporate(|id, kind| {
            assert_eq!(id, cmd);
            assert_eq!(format!("{kind:?}").to_lowercase(), cmd);
        }));
        left.expect("delivered", cmd).await;
    }
    assert!(!a.pending());
    assert!(!b.pending());
    assert!(!snapshot.accept_pause());
    assert!(!snapshot.is_attached());
    left.detach().await;
    right.send(json!({"cmd":"detach"})).await;
    session.close().await;
    f.stop().await;
}

#[tokio::test]
async fn chat_descriptor_and_invocation_share_canonical_controller_claim() {
    let f = Fixture::new(true).await;
    let (mut inbox, revoke) = f.inputs.register_with_aliases(
        identity("chat", "invocation"),
        [identity("chat", "prepared")],
        Some(7),
        |_| {},
    );
    let own = Session::new(&f.registry).await;
    own.owner.add_chat_id("exact-wire").await;
    for (first, second) in [("prepared", "invocation"), ("invocation", "prepared")] {
        let mut owner = own.open(request("chat", first)).await;
        bound(&mut owner, "chat", "invocation").await;
        let mut contender = own.open(request("chat", second)).await;
        let busy = contender.frame().await;
        assert_eq!(busy["type"], "error", "{busy}");
        assert_eq!(busy["message"], "controller_busy", "{busy}");
        owner
            .send(json!({"cmd":"message","text":"revision","req_id":"alias"}))
            .await;
        owner.expect("queued", "alias").await;
        incorporate(receive(&mut inbox).await, "alias");
        owner.expect("delivered", "alias").await;
        owner.send(json!({"cmd":"detach"})).await;
        assert!(timeout(LIMIT, socket::read_json_line(&mut owner.io))
            .await
            .unwrap()
            .unwrap()
            .is_none());
    }
    revoke.revoke();
    for route in ["prepared", "invocation"] {
        let mut peer = own.open(request("chat", route)).await;
        assert_eq!(peer.frame().await["type"], "error");
    }
    own.close().await;
}

#[tokio::test]
async fn chat_requires_trusted_owning_session_not_request_session_id() {
    let f = Fixture::new(true).await;
    let (mut inbox, _r) = f.inbox("chat", "chat-1");
    let own = Session::new(&f.registry).await;
    let other = Session::new(&f.registry).await;
    own.owner.add_chat_id("exact-wire").await;
    let mut forged = request("chat", "chat-1");
    forged["session_id"] = json!(own.id);
    let mut global = Peer::global(&f.registry, forged.clone()).await;
    assert!(global.frame().await["message"]
        .as_str()
        .unwrap()
        .contains("owning session"));
    let mut foreign = other.open(forged.clone()).await;
    assert_eq!(foreign.frame().await["type"], "error");
    let (server, client) = UnixStream::pair().unwrap();
    let (read, write) = server.into_split();
    let task = tokio::spawn(session::handle_session_aux(
        read,
        write,
        own.id.clone(),
        f.registry.clone(),
    ));
    let mut unowned = Peer {
        io: BufReader::new(client),
        task,
    };
    unowned.send(forged.clone()).await;
    assert_eq!(unowned.frame().await["type"], "error");
    assert!(!inbox.pending());
    let mut accepted = own.open(forged).await;
    bound(&mut accepted, "chat", "chat-1").await;
    accepted
        .send(json!({"cmd":"message","text":"revision","req_id":"own"}))
        .await;
    accepted.expect("queued", "own").await;
    incorporate(receive(&mut inbox).await, "own");
    accepted.expect("delivered", "own").await;
    accepted.send(json!({"cmd":"detach"})).await;
    other.close().await;
    own.close().await;
}

#[tokio::test]
async fn global_and_session_contention_cannot_be_bypassed_by_legacy_or_chat() {
    let mut f = Fixture::new(false).await;
    let mut execution = f._gremlin.next_interactive_session();
    let (interactive, _, _) = f.registry.get_run_interactive("exact-wire").await.unwrap();
    let target = interactive.target();
    let execution_id = target.stage_run_id().unwrap();
    let (inbox, _r) = f.inputs.register_with_nonce(
        identity("build", execution_id),
        Some(target.nonce()),
        |_| {},
    );
    let mut session = Session::new(&f.registry).await;
    for global_owner in [true, false] {
        let mut owner = if global_owner {
            Peer::global(&f.registry, request("build", execution_id)).await
        } else {
            session.open(request("build", execution_id)).await
        };
        bound(&mut owner, "build", execution_id).await;
        let mut contender = if global_owner {
            session.open(request("build", execution_id)).await
        } else {
            Peer::global(&f.registry, request("build", execution_id)).await
        };
        let busy = contender.frame().await;
        assert_eq!(busy["type"], "error", "{busy}");
        assert_eq!(busy["message"], "controller_busy", "{busy}");
        assert_eq!(busy["req_id"], "bind", "{busy}");
        let mut legacy = Peer::global(&f.registry, json!({"op":"attach","id":"exact-wire","nonce":target.nonce(),"scope":"conversation","stage_run_id":execution_id,"mode":"control","req_id":"legacy"})).await;
        let busy = legacy.frame().await;
        assert_eq!(busy["type"], "error", "{busy}");
        assert_eq!(busy["message"], "controller_busy", "{busy}");
        assert_eq!(busy["req_id"], "legacy", "{busy}");
        socket::write_json_line(session._main.get_mut(), &json!({"op":"chat","session_id":session.id,"text":"bypass","req_id":"chat","target":{"id":"exact-wire","nonce":target.nonce(),"stage":"build","stage_run_id":execution_id}})).await.unwrap();
        // Read the main response without consuming any execution events.
        let response = timeout(LIMIT, socket::read_json_line(&mut session._main))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response["type"], "error", "{response}");
        assert_eq!(response["message"], "controller_busy", "{response}");
        assert_eq!(response["req_id"], "chat", "{response}");
        assert!(!inbox.pending());
        assert!(!execution.accept_pause());
        assert!(!execution.is_attached());
        assert!(
            !matches!(
                timeout(Duration::from_millis(30), execution.next_command()).await,
                Ok(Some(_))
            ),
            "contention synthesized a debugger command"
        );
        owner.send(json!({"cmd":"detach"})).await;
        // An EOF frame is a deterministic barrier for claim release on aux too.
        assert!(timeout(LIMIT, socket::read_json_line(&mut owner.io))
            .await
            .unwrap()
            .unwrap()
            .is_none());
    }
    session.close().await;
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn racing_global_and_session_binds_have_one_winner_and_no_automatic_handoff() {
    let f = Fixture::new(false).await;
    let (mut inbox, _r) = f.inbox("build", "race-a");
    let session = Session::new(&f.registry).await;
    let (server, client) = UnixStream::pair().unwrap();
    let mut global = Peer {
        io: BufReader::new(client),
        task: tokio::spawn(handle_connection(
            server,
            f.registry.clone(),
            Default::default(),
        )),
    };
    let mut aux = Peer {
        io: BufReader::new(UnixStream::connect(&session.path).await.unwrap()),
        task: tokio::spawn(async {}),
    };
    let barrier = tokio::sync::Barrier::new(2);
    tokio::join!(
        async {
            barrier.wait().await;
            global.send(request("build", "race-a")).await;
        },
        async {
            barrier.wait().await;
            let mut bind = request("build", "race-a");
            bind["session_id"] = json!(session.id);
            aux.send(bind).await;
        }
    );
    let (g, s) = tokio::join!(global.frame(), aux.frame());
    let (winner, rejected, accepted, busy) = if g["type"] == "input_bound" {
        (&mut global, &mut aux, g, s)
    } else {
        (&mut aux, &mut global, s, g)
    };
    assert_eq!(accepted["type"], "input_bound", "{accepted}");
    assert_eq!(accepted["stage_run_id"], "race-a");
    assert_eq!(accepted["req_id"], "bind");
    assert_eq!(busy["type"], "error", "{busy}");
    assert_eq!(busy["message"], "controller_busy", "{busy}");
    assert_eq!(busy["req_id"], "bind");
    winner.send(json!({"cmd":"pause","req_id":"winner"})).await;
    winner.expect("queued", "winner").await;
    incorporate(receive(&mut inbox).await, "winner");
    winner.expect("delivered", "winner").await;
    winner.send(json!({"cmd":"detach"})).await;
    assert!(timeout(LIMIT, socket::read_json_line(&mut winner.io))
        .await
        .unwrap()
        .unwrap()
        .is_none());
    // Rejection closes the connection; releasing the winner cannot promote it.
    assert!(timeout(LIMIT, socket::read_json_line(&mut rejected.io))
        .await
        .unwrap()
        .unwrap()
        .is_none());
    assert!(!inbox.pending());
    let mut retry = session.open(request("build", "race-a")).await;
    bound(&mut retry, "build", "race-a").await;
    retry.send(json!({"cmd":"resume","req_id":"retry"})).await;
    retry.expect("queued", "retry").await;
    incorporate(receive(&mut inbox).await, "retry");
    retry.expect("delivered", "retry").await;
    retry.send(json!({"cmd":"detach"})).await;
    session.close().await;
    f.stop().await;
}

#[tokio::test]
async fn two_watchers_keep_output_while_exact_controllers_hold_sibling_executions() {
    use gremlins_magic_bus::{emit_corr_event, CorrId, EventKind};

    let f = Fixture::new(false).await;
    let (mut a, _ra) = f.inbox("left", "watch-a");
    let (mut b, _rb) = f.inbox("right", "watch-b");
    let session = Session::new(&f.registry).await;
    let watch = json!({"op":"watch","id":"exact-wire","stage_run_id":"watch-a"});
    let mut first = Peer::global(&f.registry, watch.clone()).await;
    let mut second = session.open(watch).await;
    assert_eq!(first.frame().await["type"], "watched");
    assert_eq!(second.frame().await["type"], "watched");
    let mut left = Peer::global(&f.registry, request("left", "watch-a")).await;
    let mut right = session.open(request("right", "watch-b")).await;
    bound(&mut left, "left", "watch-a").await;
    bound(&mut right, "right", "watch-b").await;
    for text in ["controllers held", "sibling delivered"] {
        left.send(json!({"cmd":"message","text":"revision","req_id":text}))
            .await;
        right
            .send(json!({"cmd":"message","text":"revision","req_id":text}))
            .await;
        left.expect("queued", text).await;
        right.expect("queued", text).await;
        let held = receive(&mut a).await;
        incorporate(receive(&mut b).await, text);
        right.expect("delivered", text).await;
        left.quiet().await;
        for (execution, output) in [("watch-b", "sibling only"), ("watch-a", text)] {
            emit_corr_event(
                &CorrId::new()
                    .with_run("exact-wire")
                    .with_stage_run_id(execution),
                EventKind::StreamChunk {
                    text: output.into(),
                },
            );
        }
        for watcher in [&mut first, &mut second] {
            let frame = watcher.frame().await;
            assert_eq!(frame["text"], text, "{frame}");
            assert_eq!(frame["stage_run_id"], "watch-a");
        }
        incorporate(held, text);
        left.expect("delivered", text).await;
    }
    assert!(!a.pending());
    assert!(!b.pending());
    left.detach().await;
    right.send(json!({"cmd":"detach"})).await;
    session.close().await;
    f.stop().await;
}

#[tokio::test]
async fn full_inbox_rejects_without_queued_and_connection_remains_usable() {
    let f = Fixture::new(false).await;
    let (mut inbox, _r) = f.inbox("build", "full-1");
    let sender = f.inputs.sender(&identity("build", "full-1")).unwrap();
    let mut receipts = Vec::new();
    for i in 0..INPUT_CAPACITY {
        receipts.push(sender.try_send(i.to_string(), "fill".into()).unwrap());
    }
    let mut peer = Peer::global(&f.registry, request("build", "full-1")).await;
    bound(&mut peer, "build", "full-1").await;
    peer.send(json!({"cmd":"message","text":"revision","req_id":"overflow"}))
        .await;
    let error = peer.frame().await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["req_id"], "overflow");
    assert!(error["message"].as_str().unwrap().contains("full"));
    peer.quiet().await;
    drop(receive(&mut inbox).await);
    peer.send(json!({"cmd":"message","text":"revision","req_id":"retry"}))
        .await;
    peer.expect("queued", "retry").await;
    for _ in 1..INPUT_CAPACITY {
        drop(receive(&mut inbox).await);
    }
    incorporate(receive(&mut inbox).await, "retry");
    peer.expect("delivered", "retry").await;
    peer.detach().await;
    f.stop().await;
}

#[tokio::test]
async fn revocation_drops_queued_and_held_then_ends_without_successor_leakage() {
    for held in [false, true] {
        let f = Fixture::new(false).await;
        let (mut inbox, revoke) = f.inbox("build", "old");
        let mut peer = Peer::global(&f.registry, request("build", "old")).await;
        bound(&mut peer, "build", "old").await;
        peer.send(json!({"cmd":"message","text":"revision","req_id":"old-message"}))
            .await;
        peer.expect("queued", "old-message").await;
        let command = if held {
            Some(receive(&mut inbox).await)
        } else {
            None
        };
        revoke.revoke();
        let (next, _next_revoke) = f.inbox("build", "successor");
        peer.expect("dropped", "old-message").await;
        assert_eq!(peer.frame().await["type"], "ended");
        if let Some(InputCommand::Correction(c)) = command {
            assert!(!c.incorporate(|_, _| panic!("revoked input applied")));
        }
        assert!(!next.pending());
        let mut stale = Peer::global(&f.registry, request("build", "old")).await;
        assert_eq!(stale.frame().await["type"], "error");
        let mut successor = Peer::global(&f.registry, request("build", "successor")).await;
        bound(&mut successor, "build", "successor").await;
        successor.detach().await;
        f.stop().await;
    }
}

#[tokio::test]
async fn malformed_selectors_and_bound_retargeting_never_enqueue() {
    let f = Fixture::new(false).await;
    let (inbox, _r) = f.inbox("build", "validate");
    for aux in [false, true] {
        let session = Session::new(&f.registry).await;
        for key in ["id", "stage", "stage_run_id"] {
            for value in [Value::Null, json!(42), json!(""), json!("  ")] {
                let mut invalid = request("build", "validate");
                invalid[key] = value;
                let mut peer = if aux {
                    session.open(invalid).await
                } else {
                    Peer::global(&f.registry, invalid).await
                };
                assert_eq!(peer.frame().await["type"], "error");
            }
        }
        let mut peer = if aux {
            session.open(request("build", "validate")).await
        } else {
            Peer::global(&f.registry, request("build", "validate")).await
        };
        bound(&mut peer, "build", "validate").await;
        for key in [
            "target",
            "id",
            "stage",
            "stage_run_id",
            "stage_id",
            "execution_id",
        ] {
            let mut command = json!({"cmd":"message","text":"revision","req_id":key});
            command[key] = json!("other");
            peer.send(command).await;
            peer.expect("error", key).await;
        }
        peer.quiet().await;
        assert!(!inbox.pending());
        peer.send(json!({"cmd":"detach"})).await;
        assert!(timeout(LIMIT, socket::read_json_line(&mut peer.io))
            .await
            .unwrap()
            .unwrap()
            .is_none());
        session.close().await;
    }
    f.stop().await;
}

#[tokio::test]
async fn rejection_matrix_is_reasoned_correlated_and_never_enqueues_on_either_route() {
    for aux in [false, true] {
        let mut f = Fixture::new(false).await;
        let (inbox, _revoker) = f.inbox("build", "live");
        let session = Session::new(&f.registry).await;
        let preparing = f._gremlin.next_interactive_session();
        let (interactive, _, _) = f.registry.get_run_interactive("exact-wire").await.unwrap();
        let preparing_id = interactive.target().stage_run_id().unwrap().to_owned();
        let mut missing = request("build", "live");
        missing.as_object_mut().unwrap().remove("stage_run_id");
        let mut wrong_run = request("build", "live");
        wrong_run["id"] = json!("missing-run");
        let mut missing_run = request("build", "live");
        missing_run.as_object_mut().unwrap().remove("id");
        let mut cases = vec![
            (missing_run, "id"),
            (missing, "stage_run_id"),
            (wrong_run, "not live"),
            (request("build", "stale"), "stale"),
            (request("build", &preparing_id), "preparing"),
        ];
        for (mut invalid, reason) in cases.drain(..) {
            invalid["req_id"] = json!(reason);
            let mut peer = if aux {
                session.open(invalid).await
            } else {
                Peer::global(&f.registry, invalid).await
            };
            let error = peer.frame().await;
            assert_eq!(error["type"], "error", "{error}");
            assert_eq!(error["req_id"], reason, "{error}");
            assert!(
                error["message"].as_str().unwrap().contains(reason),
                "{error}"
            );
            assert!(!inbox.pending());
        }
        drop(preparing);
        let mut unsupported = f
            ._gremlin
            .next_interactive_session_with_commands(false)
            .unwrap();
        let unsupported_id = interactive.target().stage_run_id().unwrap().to_owned();
        let mut invalid = request("build", &unsupported_id);
        invalid["req_id"] = json!("unsupported");
        let mut peer = if aux {
            session.open(invalid).await
        } else {
            Peer::global(&f.registry, invalid).await
        };
        let error = peer.frame().await;
        assert_eq!(error["type"], "error", "{error}");
        assert_eq!(error["req_id"], "unsupported", "{error}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("does not support input"),
            "{error}"
        );
        assert!(!inbox.pending());
        assert!(!unsupported.accept_pause());
        assert!(!unsupported.is_attached());
        session.close().await;
        f.stop().await;
    }
}

#[tokio::test]
async fn idle_controller_does_not_block_own_or_sibling_progress_and_closes_before_successor() {
    for aux in [false, true] {
        let mut f = Fixture::new(false).await;
        let mut idle = f._gremlin.next_interactive_session();
        idle.bind_input(&f.inputs, identity("left", "idle-a"), true, None);
        let (mut active, _active_revoke) = f.inbox("right", "active-b");
        let session = Session::new(&f.registry).await;
        let mut old = if aux {
            session.open(request("left", "idle-a")).await
        } else {
            Peer::global(&f.registry, request("left", "idle-a")).await
        };
        bound(&mut old, "left", "idle-a").await;
        let mut sibling = if aux {
            Peer::global(&f.registry, request("right", "active-b")).await
        } else {
            session.open(request("right", "active-b")).await
        };
        bound(&mut sibling, "right", "active-b").await;
        sibling
            .send(json!({"cmd":"message","text":"revision","req_id":"progress"}))
            .await;
        sibling.expect("queued", "progress").await;
        incorporate(receive(&mut active).await, "progress");
        sibling.expect("delivered", "progress").await;
        // B and both sockets remain live while A executes without input.
        let model = MockCompletionModel::from_stream_turns([
            vec![
                Event::tool_call(
                    "write-progress",
                    "Write",
                    json!({"file_path":"idle-a-progress.txt", "content":"A advanced"}),
                ),
                Event::final_response_with_default_usage(),
            ],
            vec![
                Event::tool_call("done", "Done", json!({"summary":"finished"})),
                Event::final_response_with_default_usage(),
            ],
        ]);
        let execution_a = tokio::spawn(run_agent_loop(
            model.erase(),
            "Write the progress file, then finish.",
            test_ctx(Some(f._dir.path().to_owned()), None),
            CancelToken::new(),
            loop_opts(None),
            idle,
        ));
        timeout(LIMIT, execution_a)
            .await
            .expect("execution A must progress with its input socket idle")
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(f._dir.path().join("idle-a-progress.txt")).unwrap(),
            "A advanced"
        );
        sibling
            .send(json!({"cmd":"message","text":"revision","req_id":"still-live"}))
            .await;
        sibling.expect("queued", "still-live").await;
        incorporate(receive(&mut active).await, "still-live");
        sibling.expect("delivered", "still-live").await;
        // Completing A revokes its exact inbox, while B is still live.
        let (mut successor, _successor_revoke) = f.inbox("left", "successor-a");
        assert_eq!(old.frame().await["type"], "ended");
        assert!(timeout(LIMIT, socket::read_json_line(&mut old.io))
            .await
            .unwrap()
            .unwrap()
            .is_none());
        // Even a buffered command on the retired socket cannot reach the successor.
        let _ = socket::write_json_line(
            old.io.get_mut(),
            &json!({"cmd":"message","text":"revision","req_id":"too-late"}),
        )
        .await;
        assert!(!successor.pending());
        let mut next = session.open(request("left", "successor-a")).await;
        bound(&mut next, "left", "successor-a").await;
        next.send(json!({"cmd":"message","text":"revision","req_id":"fresh"}))
            .await;
        next.expect("queued", "fresh").await;
        incorporate(receive(&mut successor).await, "fresh");
        next.expect("delivered", "fresh").await;
        assert!(!successor.pending());
        sibling.send(json!({"cmd":"detach"})).await;
        next.send(json!({"cmd":"detach"})).await;
        session.close().await;
        f.stop().await;
    }
}

#[tokio::test]
async fn eof_releases_control_without_synthesizing_commands() {
    let f = Fixture::new(false).await;
    let (mut inbox, _r) = f.inbox("build", "disconnect");
    let mut peer = Peer::global(&f.registry, request("build", "disconnect")).await;
    bound(&mut peer, "build", "disconnect").await;
    peer.send(json!({"cmd":"pause","req_id":"pause"})).await;
    peer.expect("queued", "pause").await;
    let InputCommand::Control(pause) = receive(&mut inbox).await else {
        panic!("expected pause control")
    };
    assert!(pause.incorporate(|id, kind| {
        assert_eq!(id, "pause");
        assert_eq!(kind, crate::clients::execution_input::InputControl::Pause);
    }));
    peer.expect("delivered", "pause").await;
    use tokio::io::AsyncWriteExt;
    peer.io.get_mut().shutdown().await.unwrap();
    timeout(LIMIT, &mut peer.task).await.unwrap().unwrap();
    assert!(!inbox.pending());
    let mut replacement = Peer::global(&f.registry, request("build", "disconnect")).await;
    bound(&mut replacement, "build", "disconnect").await;
    assert!(!inbox.pending());
    replacement
        .send(json!({"cmd":"resume","req_id":"resume"}))
        .await;
    replacement.expect("queued", "resume").await;
    let InputCommand::Control(resume) = receive(&mut inbox).await else {
        panic!("expected resume control")
    };
    assert!(resume.incorporate(|id, kind| {
        assert_eq!(id, "resume");
        assert_eq!(kind, crate::clients::execution_input::InputControl::Resume);
    }));
    replacement.expect("delivered", "resume").await;
    assert!(!inbox.pending());
    replacement.detach().await;
    f.stop().await;
}
