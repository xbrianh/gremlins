use super::TestRegistry;
use super::{
    chat_pair_actor, create_chat_gremlin, create_session, model, read_until_type, sandbox_dirs,
    spawn_test_actor, teardown_session,
};
use crate::executor::socket;
use crate::executor::supervisor::chat::handle_chat;
use serde_json::{json, Value};
use std::future::Future;
use std::task::Poll;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::{unix::OwnedReadHalf, unix::OwnedWriteHalf, UnixStream};

async fn main_socket(
    registry: &TestRegistry,
) -> (
    BufReader<OwnedReadHalf>,
    (OwnedWriteHalf, String),
    crate::executor::supervisor::session_task::SessionTaskHandle,
    tokio::task::JoinHandle<()>,
) {
    let (id, session) = create_session(registry).await;
    let (server, client) = UnixStream::pair().unwrap();
    let (read, write) = server.into_split();
    let task = tokio::spawn(crate::executor::supervisor::session::handle_session_main(
        read,
        gremlins_magic_bus::connection::SharedWriter::new(write),
        id.clone(),
        registry.clone(),
        session.clone(),
    ));
    let (read, write) = client.into_split();
    (BufReader::new(read), (write, id), session, task)
}

async fn send_chat(write: &mut (OwnedWriteHalf, String), mut request: Value) {
    request["session_id"] = json!(write.1);
    socket::write_json_line(&mut write.0, &request)
        .await
        .unwrap();
}

async fn frame(reader: &mut BufReader<OwnedReadHalf>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), socket::read_json_line(reader))
        .await
        .expect("socket response timed out")
        .unwrap()
        .expect("socket closed")
}

fn correction_target(started: &Value) -> Value {
    json!({"id": started["id"], "nonce": started["nonce"],
        "execution_id": started["execution_id"]})
}

async fn assert_start_snapshot(
    registry: &TestRegistry,
    session: &crate::executor::supervisor::session_task::SessionTaskHandle,
    started: &Value,
    turn: &str,
) {
    assert_eq!(started["type"], "turn_started");
    assert_eq!(started["turn"], turn);
    let current = session.get_current_turn().await.expect("active turn");
    assert_eq!(current.id, started["id"].as_str().unwrap());
    assert_eq!(current.turn.as_deref(), Some(turn));
    assert_eq!(
        current.execution_id.as_deref(),
        started["execution_id"].as_str()
    );
    let (interactive, _, _) = registry.get_run_interactive(&current.id).await.unwrap();
    let target = interactive.target();
    assert_eq!(started["nonce"], target.nonce());
    assert_eq!(started["execution_id"].as_str(), target.execution_id());
    assert!(target.execution_id().is_some_and(|id| !id.is_empty()));
}

async fn assert_no_frame(reader: &mut BufReader<OwnedReadHalf>) {
    let mut read = std::pin::pin!(socket::read_json_line(reader));
    std::future::poll_fn(|cx| {
        assert!(
            read.as_mut().poll(cx).is_pending(),
            "unexpected frame while execution is gated"
        );
        Poll::Ready(())
    })
    .await;
}

async fn wait_for_return(session: &crate::executor::supervisor::session_task::SessionTaskHandle) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if session.get_current_turn().await.is_none()
                && !session.get_chat_gremlin_keys().await.is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn must return to session");
}

#[tokio::test]
async fn second_prompt_is_gated_and_each_start_matches_live_snapshot_then_becomes_stale() {
    use crate::executor::supervisor::test_provider::pending_request;
    use crate::test_support::Sandbox;
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key": "test", "base-url": format!("http://{}", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    std::fs::write(
        sandbox.join("config/settings.yaml"),
        "default-model: openrouter/test-queued-starts\n",
    )
    .unwrap();
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let prompt = |turn| {
        json!({"op": "chat", "text": "Review draft", "turn": turn,
        "project_root": sandbox.path()})
    };
    send_chat(&mut write, prompt("first")).await;
    let ack = frame(&mut reader).await;
    assert_eq!(ack["type"], "ok");
    assert!(ack.get("nonce").is_none());
    assert!(ack.get("execution_id").is_none());
    let first = read_until_type(&mut reader, "turn_started").await;
    assert_start_snapshot(&registry, &session, &first, "first").await;
    let (_, held) = pending_request(&provider).await;

    send_chat(&mut write, prompt("second")).await;
    let busy = read_until_type(&mut reader, "error").await;
    assert!(busy.to_string().contains("session busy"), "{busy}");
    assert_start_snapshot(&registry, &session, &first, "first").await;
    session.cancel_turn(None).await.unwrap();
    drop(held);
    wait_for_return(&session).await;
    let start_gate = session.gate_turn("second", "start").await;
    let incorporation_gate = session.gate_turn("second", "incorporation").await;
    send_chat(&mut write, prompt("second")).await;
    tokio::time::timeout(Duration::from_secs(10), start_gate.entered.notified())
        .await
        .expect("accepted second prompt must reach the start gate");
    loop {
        let response = frame(&mut reader).await;
        if response["type"] == "ok" {
            assert_eq!(response["id"], first["id"]);
            assert!(response.get("nonce").is_none());
            assert!(response.get("execution_id").is_none());
            break;
        }
        assert_ne!(response["type"], "turn_started", "start escaped the gate");
    }
    assert!(session.get_current_turn().await.is_none());
    assert_eq!(
        session.get_chat_gremlin_keys().await,
        vec![first["id"].as_str().unwrap().to_string()]
    );
    let (interactive, _, _) = registry
        .get_run_interactive(first["id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        interactive.target().nonce(),
        first["nonce"].as_u64().unwrap()
    );
    assert_no_frame(&mut reader).await;
    start_gate.release.notify_one();
    let second = read_until_type(&mut reader, "turn_started").await;
    tokio::time::timeout(
        Duration::from_secs(10),
        incorporation_gate.entered.notified(),
    )
    .await
    .expect("announced execution must reach the incorporation gate");
    assert_start_snapshot(&registry, &session, &second, "second").await;
    assert_eq!(first["id"], second["id"]);
    assert_ne!(first["nonce"], second["nonce"]);
    assert_ne!(first["execution_id"], second["execution_id"]);
    let correction = "Use the revised plan in this live turn.";
    send_chat(
        &mut write,
        json!({"op": "chat", "text": correction, "req_id": "live-second-correction",
            "target": correction_target(&second)}),
    )
    .await;
    let queued = frame(&mut reader).await;
    assert_eq!(queued["type"], "ok", "{queued}");
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["id"], second["id"]);
    assert_eq!(queued["nonce"], second["nonce"]);
    assert_start_snapshot(&registry, &session, &second, "second").await;
    assert_no_frame(&mut reader).await;
    incorporation_gate.release.notify_one();
    let delivered = read_until_type(&mut reader, "delivered").await;
    assert_eq!(delivered["id"], second["id"]);
    assert_eq!(delivered["nonce"], second["nonce"]);
    assert_eq!(delivered["execution_id"], second["execution_id"]);
    assert_eq!(delivered["req_id"], "live-second-correction");
    let (incorporated, held) = pending_request(&provider).await;
    assert!(
        incorporated
            .to_string()
            .contains(&format!("[operator]: {correction}")),
        "{incorporated}"
    );
    assert_start_snapshot(&registry, &session, &second, "second").await;
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "stale correction",
        "target": correction_target(&first)}),
    )
    .await;
    let error = read_until_type(&mut reader, "error").await;
    assert!(error.to_string().contains("stale"), "{error}");
    assert_start_snapshot(&registry, &session, &second, "second").await;
    session.cancel_turn(None).await.unwrap();
    drop(held);
    wait_for_return(&session).await;
    assert!(session.get_current_turn().await.is_none());
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "returned correction",
        "target": correction_target(&second)}),
    )
    .await;
    let error = read_until_type(&mut reader, "error").await;
    assert!(
        error.to_string().contains("stale") || error.to_string().contains("no longer active"),
        "{error}"
    );
    assert!(session.get_current_turn().await.is_none());
    send_chat(&mut write, json!({"op": "close_session"})).await;
    read_until_type(&mut reader, "ok").await;
    task.await.unwrap();
}

#[tokio::test]
async fn reregistered_same_nonce_rejects_old_execution_and_invalid_targets_without_prompting() {
    use crate::clients::cancel::CancelToken;
    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let id = "chat-reregistered-correction";
    let make = || {
        create_chat_gremlin(
            id,
            &model("cmd/echo"),
            "helpful",
            &project,
            &Default::default(),
        )
        .unwrap()
    };
    let mut old = registry
        .register_chat_gremlin(id.into(), make(), CancelToken::new())
        .await;
    session.add_chat_id(id).await;
    let old_execution = old.next_interactive_session();
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let old_target = interactive.target();
    registry.stop_run(id).await.unwrap();
    drop(old_execution);
    drop(old);
    let mut replacement = registry
        .register_chat_gremlin(id.into(), make(), CancelToken::new())
        .await;
    let mut execution = replacement.next_interactive_session();
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let live = interactive.target();
    assert_eq!(old_target.nonce(), live.nonce());
    assert_ne!(old_target.execution_id(), live.execution_id());
    let target = json!({"id": id, "nonce": live.nonce(), "execution_id": live.execution_id()});
    let mut wrong_nonce = target.clone();
    wrong_nonce["nonce"] = json!(live.nonce() + 1);
    let mut wrong_execution = target.clone();
    wrong_execution["execution_id"] = json!("mismatched");
    let mut old_descriptor = target.clone();
    old_descriptor["execution_id"] = json!(old_target.execution_id());
    for (target, expected) in [
        (json!(null), "target must be an object"),
        (
            json!({"id": id, "nonce": live.nonce()}),
            "target.execution_id is required",
        ),
        (wrong_nonce, "stale"),
        (wrong_execution, "execution_id mismatch"),
        (old_descriptor, "execution_id mismatch"),
    ] {
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "must not become a prompt",
            "target": target}),
        )
        .await;
        let response = frame(&mut reader).await;
        assert_eq!(response["type"], "error", "{response}");
        assert!(response.to_string().contains(expected), "{response}");
        assert!(
            !execution.accept_pause(),
            "rejection acquired the execution"
        );
        assert!(session.get_current_turn().await.is_none());
        assert!(session.get_chat_handle().await.is_none());
        assert!(session.get_chat_gremlin_keys().await.is_empty());
    }
    drop(execution);
    let mut readonly = replacement
        .next_interactive_session_with_commands(false)
        .unwrap();
    let target = interactive.target();
    assert!(!target.accepts_commands());
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "read-only correction",
        "target": {"id": id, "nonce": target.nonce(), "execution_id": target.execution_id()}}),
    )
    .await;
    let response = frame(&mut reader).await;
    assert_eq!(response["type"], "error", "{response}");
    assert!(!readonly.accept_pause());
    assert!(session.get_current_turn().await.is_none());
    assert!(session.get_chat_handle().await.is_none());
    assert!(session.get_chat_gremlin_keys().await.is_empty());
    assert_eq!(session.get_chat_ids().await, vec![id.to_string()]);
    send_chat(&mut write, json!({"op": "close_session"})).await;
    assert_eq!(frame(&mut reader).await["type"], "ok");
    task.await.unwrap();
}

async fn assert_rejected(registry: &TestRegistry, target: Value, expected: &str) {
    let (writer, mut reader, _read, _id, session) = chat_pair_actor(registry).await;
    let request = json!({"op": "chat", "text": "correct this", "target": target});
    tokio::time::timeout(
        Duration::from_secs(10),
        handle_chat(&request, &writer, registry, &session),
    )
    .await
    .expect("invalid correction must return promptly");
    let response =
        tokio::time::timeout(Duration::from_secs(10), socket::read_json_line(&mut reader))
            .await
            .unwrap()
            .unwrap()
            .expect("correction response");
    assert_eq!(response["type"], "error", "target {target}: {response}");
    assert!(
        response.to_string().contains(expected),
        "target {target}: expected {expected:?}, got {response}"
    );
    assert!(session.get_chat_ids().await.is_empty());
    assert!(session.get_chat_gremlin_keys().await.is_empty());
    assert!(session.get_current_turn().await.is_none());
    teardown_session(&session).await;
}

#[tokio::test]
async fn correction_rejects_non_object_targets_without_starting_chat() {
    let (_sandbox, _, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    for target in [json!(null), json!(false), json!(1), json!("run"), json!([])] {
        assert_rejected(&registry, target, "target must be an object").await;
    }
}

#[tokio::test]
async fn correction_requires_nonempty_string_target_id() {
    let (_sandbox, _, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let mut target = json!({"nonce": 1, "execution_id": "execution"});
    assert_rejected(&registry, target.clone(), "target.id is required").await;
    for id in [
        json!(null),
        json!(false),
        json!(1),
        json!([]),
        json!({}),
        json!(""),
    ] {
        target["id"] = id;
        assert_rejected(&registry, target.clone(), "target.id is required").await;
    }
}

#[tokio::test]
async fn correction_requires_positive_integer_nonce() {
    let (_sandbox, _, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let mut target = json!({"id": "missing-run", "execution_id": "execution"});
    assert_rejected(&registry, target.clone(), "target.nonce is required").await;
    for nonce in [
        json!(null),
        json!(false),
        json!("1"),
        json!(0),
        json!(-1),
        json!(1.5),
        json!([]),
        json!({}),
    ] {
        target["nonce"] = nonce;
        assert_rejected(&registry, target.clone(), "target.nonce is required").await;
    }
}

#[tokio::test]
async fn correction_requires_execution_id_before_resolving_target() {
    let (_sandbox, _, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let mut target = json!({"id": "missing-run", "nonce": 1});
    assert_rejected(&registry, target.clone(), "target.execution_id is required").await;
    for execution_id in [
        json!(null),
        json!(false),
        json!(1),
        json!([]),
        json!({}),
        json!(""),
    ] {
        target["execution_id"] = execution_id;
        assert_rejected(&registry, target.clone(), "target.execution_id is required").await;
    }
}

#[tokio::test]
async fn empty_session_cannot_correct_another_sessions_chat() {
    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (_owner_id, owner) = create_session(&registry).await;
    let id = "chat-correction-owner";
    let gremlin = create_chat_gremlin(
        id,
        &model("cmd/echo"),
        "helpful",
        &project,
        &Default::default(),
    )
    .unwrap();
    owner.add_chat_id(id).await;
    let gremlin = registry
        .register_chat_gremlin(
            id.to_string(),
            gremlin,
            crate::clients::cancel::CancelToken::new(),
        )
        .await;
    owner.reinsert_gremlin(id, gremlin).await;

    assert_rejected(
        &registry,
        json!({"id": id, "nonce": 1, "execution_id": "execution"}),
        "not owned by this session",
    )
    .await;
    assert_eq!(owner.get_chat_ids().await, vec![id.to_string()]);
    assert!(owner.take_gremlin(id).await.is_some());
    teardown_session(&owner).await;
}

#[tokio::test]
async fn chat_announces_execution_identity_only_when_the_turn_starts() {
    use crate::executor::supervisor::test_provider::pending_request;
    use crate::test_support::Sandbox;
    use tokio::net::TcpListener;

    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key": "test",
            "base-url": format!("http://{}", provider.local_addr().unwrap()),
        }})
        .to_string(),
    );
    std::fs::write(
        sandbox.join("config/settings.yaml"),
        "default-model: openrouter/test-turn-started\n",
    )
    .unwrap();
    let (registry, _) = spawn_test_actor().await;
    let (writer, mut reader, _read, _session_id, session) = chat_pair_actor(&registry).await;
    handle_chat(
        &json!({
            "op": "chat", "turn": "first", "text": "Review this draft.",
            "project_root": sandbox.path(),
        }),
        &writer,
        &registry,
        &session,
    )
    .await;

    let ack = read_until_type(&mut reader, "ok").await;
    let id = ack["id"].as_str().expect("enqueue ack must name the chat");
    assert!(ack.get("nonce").is_none(), "{ack}");
    assert!(ack.get("execution_id").is_none(), "{ack}");

    let started = read_until_type(&mut reader, "turn_started").await;
    assert_eq!(started["id"], id);
    assert_eq!(started["turn"], "first");
    assert!(started["nonce"].as_u64().is_some_and(|nonce| nonce > 0));
    let execution_id = started["execution_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .expect("TurnStarted must announce an execution id");
    let current = session.get_current_turn().await.expect("active turn");
    assert_eq!(current.id, id);
    assert_eq!(current.turn.as_deref(), Some("first"));
    assert_eq!(current.execution_id.as_deref(), Some(execution_id));
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let target = interactive.target();
    assert_eq!(started["nonce"], target.nonce());
    assert_eq!(target.execution_id(), Some(execution_id));

    let (_request, _held) = pending_request(&provider).await;
    teardown_session(&session).await;
}

async fn exercise_targeted_delivery(confirm_delivery: bool) {
    use crate::clients::cancel::CancelToken;
    use crate::clients::interactive::InteractiveCommand;

    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (writer, mut reader, _read, _session_id, session) = chat_pair_actor(&registry).await;
    let id = "chat-targeted-delivery";
    let gremlin = create_chat_gremlin(
        id,
        &model("cmd/echo"),
        "helpful",
        &project,
        &Default::default(),
    )
    .unwrap();
    let mut gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    session.add_chat_id(id).await;
    let mut execution = gremlin.next_interactive_session();
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let target = interactive.target();
    let nonce = target.nonce();
    let execution_id = target.execution_id().unwrap().to_string();
    handle_chat(
        &json!({
            "op": "chat", "text": "Use the revised plan.", "req_id": "correction-1",
            "target": {"id": id, "nonce": nonce, "execution_id": execution_id},
        }),
        &writer,
        &registry,
        &session,
    )
    .await;
    let ack = read_until_type(&mut reader, "ok").await;
    assert_eq!(ack["id"], id);
    assert_eq!(ack["status"], "queued");
    assert_eq!(ack["nonce"], nonce);
    assert!(session.get_current_turn().await.is_none());
    assert!(session.get_chat_handle().await.is_none());

    assert!(execution.accept_pause());
    let command = tokio::time::timeout(Duration::from_secs(10), execution.next_command())
        .await
        .expect("correction must reach the existing execution")
        .expect("correction command");
    let InteractiveCommand::Message(text, req_id, delivery) = command else {
        panic!("expected a targeted message");
    };
    assert_eq!(text, "Use the revised plan.");
    assert_eq!(req_id.as_deref(), Some("correction-1"));
    let delivery = delivery.expect("existing delivery acknowledgement channel");
    if confirm_delivery {
        delivery.send(()).unwrap();
        let delivered = read_until_type(&mut reader, "delivered").await;
        assert_eq!(delivered["id"], id);
        assert_eq!(delivered["nonce"], nonce);
        assert_eq!(delivered["execution_id"], execution_id);
        assert_eq!(delivered["req_id"], "correction-1");
    } else {
        // Keep the sender alive: only execution end can release the delivery wait.
        drop(execution);
        let dropped = read_until_type(&mut reader, "correction_dropped").await;
        assert_eq!(dropped["id"], id);
        assert_eq!(dropped["req_id"], "correction-1");
        assert_eq!(dropped["execution_id"], execution_id);
        assert_eq!(dropped["reason"], "agent exited before incorporation");
        drop(delivery);
    }
    registry.stop_run(id).await;
    teardown_session(&session).await;
}

async fn exercise_targeted_disconnect(incorporating: bool) {
    use crate::clients::cancel::CancelToken;
    use crate::clients::interactive::{AttachError, InteractiveCommand};
    use crate::executor::supervisor::session::handle_session_main;
    use gremlins_magic_bus::connection::SharedWriter;
    use tokio::io::BufReader;
    use tokio::net::UnixStream;

    tokio::time::timeout(Duration::from_secs(10), async {
        let (_sandbox, project, _) = sandbox_dirs(None);
        let (registry, _) = spawn_test_actor().await;
        let (session_id, session) = create_session(&registry).await;
        let id = if incorporating {
            "pipeline-correction-incorporation-disconnect"
        } else {
            "pipeline-correction-readiness-disconnect"
        };
        let gremlin = create_chat_gremlin(
            id,
            &model("cmd/echo"),
            "helpful",
            &project,
            &Default::default(),
        )
        .unwrap();
        // Register a synthetic pipeline, deliberately not owned by the session.
        // Main-session teardown must not end the execution under test.
        gremlin
            .state
            .patch(&[], json!({"kind": "pipeline"}).as_object().unwrap());
        let mut gremlin = registry
            .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
            .await;
        let mut execution = gremlin.next_interactive_session();
        let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
        let target = interactive.target();
        let end = target.end_signal();

        let (server, client) = UnixStream::pair().unwrap();
        let (read, write) = server.into_split();
        let writer = SharedWriter::new(write);
        let main = tokio::spawn(handle_session_main(
            read,
            writer.clone(),
            session_id.clone(),
            registry.clone(),
            session.clone(),
        ));
        let (client_read, mut client_write) = client.into_split();
        let mut client = BufReader::new(client_read);
        socket::write_json_line(
            &mut client_write,
            &json!({
                "op": "chat", "session_id": session_id,
                "text": "Use the revised plan.", "req_id": "disconnect-correction",
                "target": {
                    "id": id, "nonce": target.nonce(),
                    "execution_id": target.execution_id(),
                },
            }),
        )
        .await
        .unwrap();
        let ack = read_until_type(&mut client, "ok").await;
        assert_eq!(ack["id"], id);
        assert_eq!(ack["status"], "queued");
        assert_eq!(ack["nonce"], target.nonce());
        assert!(matches!(target.attach(), Err(AttachError::Busy)));

        let delivery = if incorporating {
            assert!(execution.accept_pause());
            let command = execution.next_command().await.expect("correction command");
            let InteractiveCommand::Message(text, req_id, Some(delivery)) = command else {
                panic!("expected correction with incorporation acknowledgement");
            };
            assert_eq!(text, "Use the revised plan.");
            assert_eq!(req_id.as_deref(), Some("disconnect-correction"));
            Some(delivery)
        } else {
            None
        };
        drop(client);
        drop(client_write);
        writer.closed().await;
        main.await.unwrap();
        assert!(
            !end.is_ended(),
            "session teardown ended the independent target"
        );
        assert!(registry.contains_run(id).await);

        // Await lease release, not socket activity or a wall-clock sleep.
        let replacement = loop {
            match target.attach() {
                Ok(attachment) => break attachment,
                Err(AttachError::Busy) => tokio::task::yield_now().await,
                Err(error) => panic!("live execution became unavailable: {error:?}"),
            }
        };
        if let Some(delivery) = delivery {
            assert!(
                delivery.send(()).is_err(),
                "disconnected delivery wait survived"
            );
        }
        assert!(
            execution.accept_pause(),
            "stale correction cleanup cleared the new owner"
        );
        assert!(execution.is_attached());
        assert!(matches!(target.attach(), Err(AttachError::Busy)));
        assert!(session.get_chat_ids().await.is_empty());
        drop(replacement);
        registry.stop_run(id).await.unwrap();
    })
    .await
    .expect("disconnect must release the targeted correction attachment");
}

#[tokio::test]
async fn main_socket_disconnect_releases_targeted_correction_during_readiness() {
    exercise_targeted_disconnect(false).await;
}

#[tokio::test]
async fn main_socket_disconnect_releases_targeted_correction_during_incorporation() {
    exercise_targeted_disconnect(true).await;
}

#[tokio::test]
async fn targeted_correction_reports_delivery_through_existing_oneshot() {
    exercise_targeted_delivery(true).await;
}

#[tokio::test]
async fn execution_end_cancels_targeted_correction_delivery_wait() {
    exercise_targeted_delivery(false).await;
}

#[tokio::test]
async fn execution_end_before_chat_readiness_drops_correction_without_retargeting() {
    use crate::clients::cancel::CancelToken;
    use crate::clients::interactive::AttachError;

    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let id = "chat-readiness-execution-end";
    let gremlin = create_chat_gremlin(
        id,
        &model("cmd/echo"),
        "helpful",
        &project,
        &Default::default(),
    )
    .unwrap();
    let mut gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    session.add_chat_id(id).await;
    let execution = gremlin.next_interactive_session();
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let target = interactive.target();
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "Do not retry this correction",
        "req_id": "ended-before-ready", "target": {
            "id": id, "nonce": target.nonce(),
            "execution_id": target.execution_id(),
        }}),
    )
    .await;
    let ack = frame(&mut reader).await;
    assert_eq!(ack["type"], "ok");
    assert_eq!(ack["status"], "queued");
    assert!(matches!(target.attach(), Err(AttachError::Busy)));

    // Never accept the reserved pause. Keep the owning socket open.
    drop(execution);
    let mut successor = gremlin.next_interactive_session();
    let dropped = frame(&mut reader).await;
    assert_eq!(dropped["type"], "correction_dropped");
    assert_eq!(dropped["id"], id);
    assert_eq!(dropped["req_id"], "ended-before-ready");
    assert_eq!(dropped["execution_id"], target.execution_id().unwrap());
    assert_eq!(dropped["reason"], "agent exited before accepting pause");
    assert!(!successor.accept_pause(), "correction paused the successor");
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(100), successor.next_command()).await,
            Ok(Some(_))
        ),
        "correction command leaked into the successor"
    );
    assert!(!successor.is_attached());

    send_chat(&mut write, json!({"op": "close_session"})).await;
    loop {
        let response =
            tokio::time::timeout(Duration::from_secs(10), socket::read_json_line(&mut reader))
                .await
                .expect("session must close after correction terminates")
                .unwrap();
        let Some(response) = response else { break };
        assert_ne!(
            response["type"], "delivered",
            "ended correction was delivered"
        );
        assert_ne!(
            response["type"], "correction_dropped",
            "duplicate terminal response"
        );
    }
    task.await.unwrap();
}
