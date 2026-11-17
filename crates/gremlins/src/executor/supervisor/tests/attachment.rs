//! Tests for `handle_conversation_attach`: the shared conversation-scope
//! attach handler. Covers leaf tracking, disconnect before/after pause
//! acknowledgment, command-queue backpressure, and re-attachment.
//!
//! These tests call `handle_conversation_attach` directly on `UnixStream::pair`
//! so they exercise the full handshake, command loop, and bus subscription path
//! without going through the socket-routing layer.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run, EventKind, Recv};
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::time::timeout;

use super::{create_chat_gremlin, model, spawn_test_actor};
use crate::clients::backend::RunParams;
use crate::clients::error::ClientError;
use crate::clients::interactive::InteractiveCommand;
use crate::definition::StaticDefinition;
use crate::executor::gremlin::{Gremlin, GremlinConfig};
use crate::executor::socket;
use crate::executor::supervisor::attachment::handle_conversation_attach;
use crate::executor::supervisor::run_registry::RunRegistry;
use crate::executor::supervisor::test_provider::pending_request;
use crate::executor::supervisor::CancelToken;
use crate::test_support::Sandbox;

// ── Helpers shared by both test groups ──────────────────────────────────

/// Returns a `BufReader` connected to a spawned handler (fire-and-forget).
fn connect(registry: &Arc<RunRegistry>, id: &str) -> BufReader<UnixStream> {
    let (server, client) = UnixStream::pair().unwrap();
    let request = json!({"op": "attach", "scope": "conversation", "id": id});
    let id = id.to_string();
    let registry = registry.clone();
    tokio::spawn(async move {
        let (read, write) = server.into_split();
        handle_conversation_attach(
            &request,
            &id,
            &registry,
            BufReader::new(read),
            &SharedWriter::new(write),
        )
        .await;
    });
    BufReader::new(client)
}

/// Returns a `BufReader` and a `Future` for the spawned handler (for
/// tests that need to await or cancel the handler task).
fn connection(
    registry: &Arc<RunRegistry>,
    id: &str,
) -> (BufReader<UnixStream>, impl Future<Output = ()> + 'static) {
    let (client, server) = UnixStream::pair().unwrap();
    let registry = registry.clone();
    let request = json!({"op": "attach", "scope": "conversation", "id": id});
    let id = id.to_string();
    let handler = async move {
        let (read, write) = server.into_split();
        handle_conversation_attach(
            &request,
            &id,
            &registry,
            BufReader::new(read),
            &SharedWriter::new(write),
        )
        .await;
    };
    (BufReader::new(client), handler)
}

/// Next frame that carries an outcome: skips progress (`status`) and
/// acknowledgment (`attached`) frames.
async fn next_response(reader: &mut BufReader<UnixStream>) -> Value {
    loop {
        let msg = socket::read_json_line(reader).await.unwrap().unwrap();
        if msg["type"] != "status" && msg["type"] != "attached" {
            return msg;
        }
    }
}

/// Read frames until one satisfies `matches`, ignoring display events.
async fn wait_for(client: &mut BufReader<UnixStream>, matches: impl Fn(&Value) -> bool) {
    loop {
        let frame = socket::read_json_line(client)
            .await
            .unwrap()
            .expect("attach connection ended");
        assert_ne!(frame["type"], "error", "{frame}");
        if matches(&frame) {
            return;
        }
    }
}

// ── Leaf-tracking / sequence tests (was debug/sequence_tests.rs) ───────

#[tokio::test]
async fn debug_sequence_tracks_the_active_leaf_and_routes_to_its_model() {
    let model = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = json!({"openai": {
        "api-key": "test",
        "base-url": format!("http://{}/v1", model.local_addr().unwrap())
    }});
    let sandbox = Sandbox::with_providers(&provider.to_string());
    tokio::time::timeout(Duration::from_secs(15), drive_sequence(&sandbox, model))
        .await
        .expect("timed out exercising sequence debug attachment");
}

async fn drive_sequence(sandbox: &Sandbox, model: TcpListener) {
    // A TCP read in the exec child is an explicit gate, not a timing window.
    let gate = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cmd = format!(
        "read -r go </dev/tcp/127.0.0.1/{}",
        gate.local_addr().unwrap().port()
    );
    // A cmd-backed agent can finish without ever accepting a pause. Its
    // unacknowledged request must not reach the following native agent.
    let gated_model = format!("cmd/bash -c '{cmd}'");
    let pipeline = json!({
        "default_model": "openai/debug-sequence-test",
        "stages": [{"name":"pipeline", "type":"sequence", "body":[
            {"name":"prepare", "type":"sequence", "body":[
                {"name":"work", "type":"exec", "options":{"cmds":[cmd]}}
            ]},
            {"name":"preflight", "type":"agent", "model":gated_model, "prompt":["Preflight."]},
            {"name":"review", "type":"sequence", "body":[
                {"name":"work", "type":"agent", "prompt":["Review the changes."]}
            ]}
        ]}]
    });
    let definition =
        StaticDefinition::from_expanded_bytes(&serde_json::to_vec(&pipeline).unwrap(), None, None)
            .unwrap();
    let gremlin = Gremlin::init(
        "sequence-debug",
        &sandbox.join("sequence.json5"),
        &definition,
        &Default::default(),
        None,
        &GremlinConfig {
            ephemeral: true,
            ..Default::default()
        },
    )
    .unwrap();
    let store = gremlin.state.store_handle();
    let registry = Arc::new(RunRegistry::new(tokio::sync::watch::channel(false).0));
    let id = registry
        .launch_run(gremlin, "pipeline".into(), None, "sequence".into(), false)
        .await;

    let (mut exec, _) = gate.accept().await.unwrap();
    let assert_stage = || {
        let tree = store.state_tree();
        assert_eq!(tree["stage"], "pipeline");
        assert_eq!(tree["sub_stage"], "work");
    };
    assert_stage();
    let rejected = next_response(&mut connect(&registry, &id)).await;
    assert_eq!(rejected["type"], "error");
    assert!(rejected["message"]
        .as_str()
        .unwrap()
        .contains("not in an agent stage"));
    exec.write_all(b"go\n").await.unwrap();

    let (mut preflight, _) = gate.accept().await.unwrap();
    let mut expiring = connect(&registry, &id);
    // The ack is written just before the pause is requested; the gate keeps
    // the cmd-backed agent from accepting it so the execution can still exit.
    let attached = socket::read_json_line(&mut expiring)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attached["type"], "attached", "{attached}");
    preflight.write_all(b"go\n").await.unwrap();
    let expired = next_response(&mut expiring).await;
    assert_eq!(expired["type"], "error", "{expired}");

    let (initial, _held) = pending_request(&model).await;
    assert!(initial.to_string().contains("Review the changes."));
    assert_stage();
    let mut debug = connect(&registry, &id);
    let ready = next_response(&mut debug).await;
    assert_eq!(ready["type"], "ready", "{ready}");
    assert_eq!(ready["scope"], "conversation");
    assert_eq!(ready["id"].as_str(), Some(id.as_str()));
    socket::write_json_line(
        debug.get_mut(),
        &json!({"op":"message", "text":"review the corrected implementation"}),
    )
    .await
    .unwrap();
    let (body, _corrected) = pending_request(&model).await;
    assert!(body
        .to_string()
        .contains("[operator]: review the corrected implementation"));
    registry.stop_run(&id).await.unwrap();
}

// ── Detach / reattach tests (was tests/debug_detach.rs) ─────────────────

async fn exercise_disconnect(id: &str, accepted: bool) {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key": "test", "base-url": format!("http://{}", provider.local_addr().unwrap()),
        }})
        .to_string(),
    );
    timeout(
        Duration::from_secs(15),
        drive_handshake(&sandbox, &provider, id, accepted),
    )
    .await
    .expect("timed out disconnecting and reattaching");
}

async fn drive_handshake(sandbox: &Sandbox, provider: &TcpListener, id: &str, accepted: bool) {
    let spec = format!("openrouter/{id}");
    let gremlin = create_chat_gremlin(
        id,
        &model(&spec),
        "helpful",
        sandbox.path(),
        &Default::default(),
    )
    .unwrap();
    let cancel = CancelToken::new();
    let (registry, _) = spawn_test_actor().await;
    let mut gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, cancel.clone())
        .await;
    gremlin.state.set_stage("chat-0001", None, "");
    let session = gremlin.next_interactive_session();
    let params = RunParams {
        prompt: "Review this.".into(),
        cwd: Some(sandbox.path().to_path_buf()),
        gremlin_id: Some(id.into()),
        cancel_token: Some(cancel.clone()),
        ..RunParams::empty()
    };
    let agent = gremlin.client.run(params, session);
    tokio::pin!(agent);

    let (mut first, handler) = connection(&registry, id);
    tokio::pin!(handler);
    tokio::select! {
        _ = &mut handler => panic!("attach handshake ended before requesting pause"),
        // Wait for the status frame that follows the attached ack — this
        // guarantees the pause request has been registered (target.attach()
        // was called and succeeded) before the test proceeds, regardless
        // of how the runtime schedules the handler poll.
        _ = wait_for(&mut first, |frame| {
            frame["type"] == "status" && frame["stage"] == "waiting_for_ready"
        }) => {}
    }
    // The pause is registered but may not yet be observed by the agent.
    // Neither future is spawned. Leave the handler unpolled; advance only the
    // agent to its Ready broadcast, or don't let it observe the pause at all.
    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    if accepted {
        tokio::select! {
            _ = &mut agent => panic!("agent exited before accepting pause"),
            _ = async {
                loop {
                    let Recv::Event(event) = events.recv().await else { panic!("lost run events") };
                    if matches!(event.kind(), EventKind::Ready { .. }) { break; }
                }
            } => {}
        }
    }
    drop(first);
    handler.await;

    // A request to the held provider proves the agent resumed without input.
    let (_request, _held) = tokio::select! {
        _ = &mut agent => panic!("agent exited instead of resuming"),
        first = pending_request(provider) => first,
    };
    if !accepted {
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.kind(), EventKind::Ready { .. }),
                "the withdrawn pause was acknowledged before the first model request"
            );
        }
    }
    let (mut second, handler) = connection(&registry, id);
    let handler = tokio::spawn(handler);
    tokio::select! {
        _ = &mut agent => panic!("agent exited instead of pausing again"),
        _ = wait_for(&mut second, |frame| {
            frame["type"] == "ready" && frame["id"].as_str() == Some(id)
        }) => {}
    }
    // If a stale Quit released the second pause, the next request lacks the
    // correction, even if Ready raced a pending write on the attach socket.
    socket::write_json_line(
        second.get_mut(),
        &json!({"op": "message", "text": "try revision two"}),
    )
    .await
    .unwrap();
    let (corrected, _held) = tokio::select! {
        _ = &mut agent => panic!("agent exited before consuming second operator's command"),
        next = pending_request(provider) => next,
    };
    assert!(
        corrected
            .to_string()
            .contains("[operator]: try revision two"),
        "{corrected}"
    );
    cancel.cancel();
    assert!(matches!(agent.await, Err(ClientError::Cancelled)));
    drop(second);
    handler.await.unwrap();
    registry.stop_run(id).await.unwrap();
}

#[tokio::test]
async fn full_debug_queue_does_not_block_disconnect_and_reattach() {
    timeout(Duration::from_secs(10), async {
        let id = "debug-full-queue";
        let dir = tempfile::tempdir().unwrap();
        let gremlin = create_chat_gremlin(
            id,
            &model("cmd/echo"),
            "helpful",
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        let (registry, _) = spawn_test_actor().await;
        let mut gremlin = registry
            .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
            .await;
        gremlin.state.set_stage("chat-0001", None, "");
        let mut session = gremlin.next_interactive_session();
        let (mut client, handler) = connection(&registry, id);
        let handler = tokio::spawn(handler);
        session.paused().await;
        let step = json!({"op":"continue"});
        let (command, ()) = tokio::join!(session.next_command(), async {
            wait_for(&mut client, |frame| {
                frame["type"] == "ready" && frame["id"].as_str() == Some(id)
            })
            .await;
            socket::write_json_line(client.get_mut(), &step)
                .await
                .unwrap();
        });
        assert!(matches!(command, Some(InteractiveCommand::RunTurn)));

        // Keep the step in flight: no further commands can be consumed.
        for _ in 0..9 {
            socket::write_json_line(client.get_mut(), &step)
                .await
                .unwrap();
        }
        let rejection = socket::read_json_line(&mut client).await.unwrap().unwrap();
        assert_eq!(rejection["type"], "error");
        assert!(rejection["message"]
            .as_str()
            .unwrap()
            .contains("queue is full"));
        drop(client);
        handler.await.unwrap();

        let (mut replacement, handler) = connection(&registry, id);
        let handler = tokio::spawn(handler);
        // A fresh attachment must be able to request its own pause.
        wait_for(&mut replacement, |frame| {
            frame["type"] == "status" && frame["stage"] == "waiting_for_ready"
        })
        .await;
        drop(replacement);
        handler.await.unwrap();
        registry.stop_run(id).await.unwrap();
    })
    .await
    .expect("full command queue stranded the debugger");
}

#[tokio::test]
async fn debug_detach_before_agent_observes_pause() {
    exercise_disconnect("detach-before-pause", false).await;
}

#[tokio::test]
async fn debug_detach_after_agent_emits_unread_ready() {
    exercise_disconnect("detach-unread-ready", true).await;
}
