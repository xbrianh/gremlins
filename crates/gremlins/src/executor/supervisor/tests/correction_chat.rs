use super::TestRegistry;
use super::{
    create_chat_gremlin, create_session, model, read_until_type, sandbox_dirs, spawn_test_actor,
    teardown_session,
};
use crate::executor::socket;
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
    // Follow the owner's identity change while the second execution is held live.
    for (nonce, execution_id, expected) in [
        (
            second["nonce"].as_u64().unwrap() + 1,
            second["execution_id"].as_str().unwrap(),
            "error",
        ),
        (
            second["nonce"].as_u64().unwrap(),
            first["execution_id"].as_str().unwrap(),
            "error",
        ),
        (
            second["nonce"].as_u64().unwrap(),
            second["execution_id"].as_str().unwrap(),
            "attached",
        ),
    ] {
        let (client, server) = UnixStream::pair().unwrap();
        let (read, aux_write) = server.into_split();
        let aux = tokio::spawn(crate::executor::supervisor::session::handle_session_aux(
            read,
            aux_write,
            write.1.clone(),
            registry.clone(),
        ));
        let mut client = BufReader::new(client);
        socket::write_json_line(
            client.get_mut(),
            &json!({
                "op": "attach", "scope": "conversation", "session_id": write.1,
                "id": second["id"], "nonce": nonce, "execution_id": execution_id,
            }),
        )
        .await
        .unwrap();
        let response = socket::read_json_line(&mut client).await.unwrap().unwrap();
        assert_eq!(response["type"], expected, "{response}");
        if expected == "attached" {
            assert_eq!(response["nonce"], second["nonce"]);
            assert_eq!(response["execution_id"], second["execution_id"]);
        }
        drop(client);
        aux.await.unwrap();
    }
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

#[tokio::test]
async fn correction_rejects_malformed_targets_without_starting_chat() {
    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
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
    #[rustfmt::skip]
    let cases = [
        ("target/null", json!(null), "target must be an object"),
        ("target/bool", json!(false), "target must be an object"),
        ("target/number", json!(1), "target must be an object"),
        ("target/string", json!("run"), "target must be an object"),
        ("target/array", json!([]), "target must be an object"),
        ("id/missing", json!({"nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("id/null", json!({"id": null, "nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("id/bool", json!({"id": false, "nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("id/number", json!({"id": 1, "nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("id/array", json!({"id": [], "nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("id/object", json!({"id": {}, "nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("id/empty", json!({"id": "", "nonce": 1, "execution_id": "execution"}), "target.id is required"),
        ("nonce/missing", json!({"id": "missing-run", "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/null", json!({"id": "missing-run", "nonce": null, "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/bool", json!({"id": "missing-run", "nonce": false, "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/string", json!({"id": "missing-run", "nonce": "1", "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/zero", json!({"id": "missing-run", "nonce": 0, "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/negative", json!({"id": "missing-run", "nonce": -1, "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/fraction", json!({"id": "missing-run", "nonce": 1.5, "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/array", json!({"id": "missing-run", "nonce": [], "execution_id": "execution"}), "target.nonce is required"),
        ("nonce/object", json!({"id": "missing-run", "nonce": {}, "execution_id": "execution"}), "target.nonce is required"),
        ("execution_id/missing", json!({"id": "missing-run", "nonce": 1}), "target.execution_id is required"),
        ("execution_id/null", json!({"id": "missing-run", "nonce": 1, "execution_id": null}), "target.execution_id is required"),
        ("execution_id/bool", json!({"id": "missing-run", "nonce": 1, "execution_id": false}), "target.execution_id is required"),
        ("execution_id/number", json!({"id": "missing-run", "nonce": 1, "execution_id": 1}), "target.execution_id is required"),
        ("execution_id/array", json!({"id": "missing-run", "nonce": 1, "execution_id": []}), "target.execution_id is required"),
        ("execution_id/object", json!({"id": "missing-run", "nonce": 1, "execution_id": {}}), "target.execution_id is required"),
        ("execution_id/empty", json!({"id": "missing-run", "nonce": 1, "execution_id": ""}), "target.execution_id is required"),
        ("another session owns the chat", json!({"id": id, "nonce": 1, "execution_id": "execution"}), "not owned by this session"),
    ];
    for (case, target, expected) in cases {
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "correct this", "target": target}),
        )
        .await;
        let response = frame(&mut reader).await;
        assert_eq!(
            response["type"], "error",
            "{case}: target {target}: {response}"
        );
        assert!(
            response.to_string().contains(expected),
            "{case}: target {target}: expected {expected:?}, got {response}"
        );
        assert!(session.get_chat_ids().await.is_empty(), "{case}: chat ids");
        assert!(
            session.get_chat_gremlin_keys().await.is_empty(),
            "{case}: gremlin keys"
        );
        assert!(
            session.get_current_turn().await.is_none(),
            "{case}: current turn"
        );
        assert!(
            session.get_chat_handle().await.is_none(),
            "{case}: chat handle"
        );
    }
    assert_eq!(owner.get_chat_ids().await, vec![id.to_string()]);
    assert!(owner.take_gremlin(id).await.is_some());
    send_chat(&mut write, json!({"op": "close_session"})).await;
    assert_eq!(frame(&mut reader).await["type"], "ok");
    task.await.unwrap();
    teardown_session(&owner).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CorrectionWait {
    Readiness,
    Incorporation,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CorrectionFailure {
    MainSocketEof,
    ExecutionEnd,
}

async fn exercise_targeted_failure(phase: CorrectionWait, trigger: CorrectionFailure) {
    use crate::clients::cancel::CancelToken;
    use crate::clients::interactive::{AttachError, InteractiveCommand};

    tokio::time::timeout(Duration::from_secs(10), async {
        let (_sandbox, project, _) = sandbox_dirs(None);
        let (registry, _) = spawn_test_actor().await;
        let (mut reader, mut write, session, task) = main_socket(&registry).await;
        let id = "pipeline-targeted-failure";
        let gremlin = create_chat_gremlin(
            id,
            &model("cmd/echo"),
            "helpful",
            &project,
            &Default::default(),
        )
        .unwrap();
        // Independent pipeline: MAIN teardown must not end this execution.
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
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "Use the revised plan.",
            "req_id": "correction-1", "target": {
                "id": id, "nonce": target.nonce(),
                "execution_id": target.execution_id(),
            }}),
        )
        .await;
        let ack = frame(&mut reader).await;
        assert_eq!(ack["type"], "ok");
        assert_eq!(ack["id"], id);
        assert_eq!(ack["status"], "queued");
        assert_eq!(ack["nonce"], target.nonce());
        assert!(session.get_current_turn().await.is_none());
        assert!(session.get_chat_handle().await.is_none());
        assert!(session.get_chat_ids().await.is_empty());
        assert!(matches!(target.attach(), Err(AttachError::Busy)));

        let delivery = if phase == CorrectionWait::Incorporation {
            assert!(execution.accept_pause());
            let command = execution.next_command().await.expect("correction command");
            let InteractiveCommand::Message(text, req_id, Some(delivery)) = command else {
                panic!("expected correction with incorporation acknowledgement");
            };
            assert_eq!(text, "Use the revised plan.");
            assert_eq!(req_id.as_deref(), Some("correction-1"));
            Some(delivery)
        } else {
            None
        };
        if trigger == CorrectionFailure::MainSocketEof {
            drop(write);
            task.await.unwrap();
            assert!(
                !end.is_ended(),
                "MAIN teardown ended the independent target"
            );
            assert!(registry.contains_run(id).await);
            // Reacquire the SAME live target, not a successor, after EOF.
            let replacement = loop {
                match target.attach() {
                    Ok(attachment) => break attachment,
                    Err(AttachError::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("live execution became unavailable: {error:?}"),
                }
            };
            if let Some(delivery) = delivery {
                assert!(delivery.send(()).is_err(), "EOF delivery wait survived");
            }
            assert!(execution.accept_pause(), "cleanup cleared the new owner");
            assert!(execution.is_attached());
            assert!(matches!(target.attach(), Err(AttachError::Busy)));
            // No terminal frame can be emitted after MAIN EOF.
            while let Some(response) = socket::read_json_line(&mut reader).await.unwrap() {
                assert_ne!(response["type"], "delivered");
                assert_ne!(response["type"], "correction_dropped");
            }
            drop(replacement);
        } else {
            // Keep MAIN open and the delivery sender alive until execution end.
            drop(execution);
            let mut successor = gremlin.next_interactive_session();
            let dropped = frame(&mut reader).await;
            assert_eq!(dropped["type"], "correction_dropped");
            assert_eq!(dropped["id"], id);
            assert_eq!(dropped["req_id"], "correction-1");
            assert_eq!(dropped["execution_id"], target.execution_id().unwrap());
            assert_eq!(
                dropped["reason"],
                if phase == CorrectionWait::Incorporation {
                    "agent exited before incorporation"
                } else {
                    "agent exited before accepting pause"
                }
            );
            if let Some(delivery) = delivery {
                assert!(delivery.send(()).is_err(), "ended delivery wait survived");
            }
            assert!(!successor.accept_pause(), "correction paused the successor");
            assert!(!successor.is_attached());
            let mut command = std::pin::pin!(successor.next_command());
            // Give delayed correction work repeated opportunities to leak a command.
            for _ in 0..128 {
                tokio::task::yield_now().await;
                let ended = std::future::poll_fn(|cx| {
                    Poll::Ready(match command.as_mut().poll(cx) {
                        Poll::Ready(Some(command)) => {
                            panic!("successor received a command: {command:?}")
                        }
                        Poll::Ready(None) => true,
                        Poll::Pending => false,
                    })
                })
                .await;
                if ended {
                    break;
                }
            }
            send_chat(&mut write, json!({"op": "close_session"})).await;
            while let Some(response) = socket::read_json_line(&mut reader).await.unwrap() {
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
        registry.stop_run(id).await.unwrap();
    })
    .await
    .expect("targeted correction failure must release its attachment");
}

#[tokio::test]
async fn main_socket_disconnect_releases_targeted_correction_during_readiness() {
    exercise_targeted_failure(CorrectionWait::Readiness, CorrectionFailure::MainSocketEof).await;
}

#[tokio::test]
async fn main_socket_disconnect_releases_targeted_correction_during_incorporation() {
    exercise_targeted_failure(
        CorrectionWait::Incorporation,
        CorrectionFailure::MainSocketEof,
    )
    .await;
}

#[tokio::test]
async fn execution_end_cancels_targeted_correction_delivery_wait() {
    exercise_targeted_failure(
        CorrectionWait::Incorporation,
        CorrectionFailure::ExecutionEnd,
    )
    .await;
}

#[tokio::test]
async fn execution_end_before_chat_readiness_drops_correction_without_retargeting() {
    exercise_targeted_failure(CorrectionWait::Readiness, CorrectionFailure::ExecutionEnd).await;
}
