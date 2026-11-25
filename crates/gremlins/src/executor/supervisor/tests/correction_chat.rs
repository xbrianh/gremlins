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

fn assert_new_identity_only(frame: &Value) {
    assert!(
        frame["stage_run_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{frame}"
    );
    assert!(frame.get("execution_id").is_none(), "{frame}");
    assert!(frame.get("stage_id").is_none(), "{frame}");
}

fn correction_target(started: &Value) -> Value {
    json!({"id": started["id"], "nonce": started["nonce"],
        "stage_run_id": started["stage_run_id"]})
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
        current.stage_run_id.as_deref(),
        started["stage_run_id"].as_str()
    );
    let (interactive, _, _) = registry.get_run_interactive(&current.id).await.unwrap();
    let target = interactive.target();
    assert_eq!(started["nonce"], target.nonce());
    assert_eq!(started["stage_run_id"].as_str(), target.stage_run_id());
    assert!(target.stage_run_id().is_some_and(|id| !id.is_empty()));
    assert_new_identity_only(started);
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
    let (id, session) = create_session(&registry).await;
    let socket_dir = tempfile::tempdir().unwrap();
    let socket_path = socket_dir.path().join("session.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let task = tokio::spawn(crate::executor::supervisor::session::run_session_listener(
        listener,
        id.clone(),
        registry.clone(),
        session.clone(),
    ));
    let (read, write) = UnixStream::connect(&socket_path)
        .await
        .unwrap()
        .into_split();
    let mut reader = BufReader::new(read);
    let mut write = (write, id);
    let prompt = |turn| {
        json!({"op": "chat", "text": "Review draft", "turn": turn,
        "project_root": sandbox.path()})
    };
    send_chat(&mut write, prompt("first")).await;
    let ack = frame(&mut reader).await;
    assert_eq!(ack["type"], "ok");
    assert!(ack.get("nonce").is_none());
    assert!(ack.get("stage_run_id").is_none());
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
            assert!(response.get("stage_run_id").is_none());
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
    assert_ne!(first["stage_run_id"], second["stage_run_id"]);
    let correction = "Use the revised plan in this live turn.";
    send_chat(
        &mut write,
        json!({"op": "chat", "text": correction, "req_id": "live-second-correction",
            "target": correction_target(&second)}),
    )
    .await;
    // A published debugger target is not an execution inbox. Before the
    // runner registers input, rejection must not promise queued delivery.
    let rejected = frame(&mut reader).await;
    assert_eq!(rejected["type"], "error", "{rejected}");
    assert_start_snapshot(&registry, &session, &second, "second").await;
    assert_no_frame(&mut reader).await;
    incorporation_gate.release.notify_one();
    let (_, held) = pending_request(&provider).await;
    // Follow the owner's identity change while the second execution is held live.
    for (nonce, stage_run_id, expected) in [
        (
            second["nonce"].as_u64().unwrap() + 1,
            second["stage_run_id"].as_str().unwrap(),
            "error",
        ),
        (
            second["nonce"].as_u64().unwrap(),
            first["stage_run_id"].as_str().unwrap(),
            "error",
        ),
        (
            second["nonce"].as_u64().unwrap(),
            second["stage_run_id"].as_str().unwrap(),
            "attached",
        ),
    ] {
        // The real listener supplies the verified session owner; the unowned
        // aux entry point rejects even a valid conversation target.
        let client = UnixStream::connect(&socket_path).await.unwrap();
        let mut client = BufReader::new(client);
        socket::write_json_line(
            client.get_mut(),
            &json!({
                "op": "attach", "scope": "conversation", "session_id": write.1,
                "id": second["id"], "nonce": nonce, "stage_run_id": stage_run_id,
            }),
        )
        .await
        .unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(10), socket::read_json_line(&mut client))
                .await
                .expect("auxiliary attach response timed out")
                .unwrap()
                .unwrap();
        assert_eq!(response["type"], expected, "{response}");
        if expected == "attached" {
            assert_eq!(response["nonce"], second["nonce"]);
            assert_eq!(response["stage_run_id"], second["stage_run_id"]);
            assert_new_identity_only(&response);
        }
        drop(client);
    }
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
    assert_ne!(old_target.stage_run_id(), live.stage_run_id());
    let (mut inbox, _revoker) = replacement.execution_inputs.register_with_nonce(
        crate::clients::execution_input::ExecutionIdentity {
            run_id: id.into(),
            stage_name: "build".into(),
            stage_run_id: live.stage_run_id().unwrap().into(),
        },
        Some(live.nonce()),
        |_| {},
    );
    let target = json!({"id": id, "nonce": live.nonce(), "stage_run_id": live.stage_run_id()});
    let mut wrong_nonce = target.clone();
    wrong_nonce["nonce"] = json!(live.nonce() + 1);
    let mut wrong_execution = target.clone();
    wrong_execution["stage_run_id"] = json!("mismatched");
    let mut old_descriptor = target.clone();
    old_descriptor["stage_run_id"] = json!(old_target.stage_run_id());
    for (target, expected) in [
        (wrong_nonce, "stale"),
        (wrong_execution, "stale"),
        (old_descriptor, "stale"),
    ] {
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "must not become a prompt",
            "target": target}),
        )
        .await;
        let response = frame(&mut reader).await;
        assert_eq!(response["type"], "error", "{response}");
        assert!(
            response.to_string().contains(expected)
                || (expected == "stale" && response.to_string().contains("stage_run_id mismatch")),
            "{response}"
        );
        assert!(
            !execution.accept_pause(),
            "rejection acquired the execution"
        );
        assert!(inbox.try_recv().is_err(), "rejected target enqueued input");
        assert!(session.get_current_turn().await.is_none());
        assert!(session.get_chat_handle().await.is_none());
        assert!(session.get_chat_gremlin_keys().await.is_empty());
    }
    drop(inbox);
    drop(execution);
    let mut readonly = replacement
        .next_interactive_session_with_commands(false)
        .unwrap();
    let target = interactive.target();
    assert!(!target.accepts_commands());
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "read-only correction",
        "target": {"id": id, "nonce": target.nonce(), "stage_run_id": target.stage_run_id()}}),
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
async fn retired_target_keys_are_rejected_before_resolution_without_delivery() {
    use crate::clients::cancel::CancelToken;

    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let id = "pipeline-retired-correction";
    let gremlin = create_chat_gremlin(
        id,
        &model("cmd/echo"),
        "helpful",
        &project,
        &Default::default(),
    )
    .unwrap();
    gremlin
        .state
        .patch(&[], json!({"kind": "pipeline"}).as_object().unwrap());
    let mut gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    let mut execution = gremlin.next_interactive_session();
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let live = interactive.target();
    for key in ["execution_id", "stage_id"] {
        for with_new_key in [false, true] {
            // The missing run proves retired-key validation precedes resolution;
            // the live run proves rejection never acquires a pause or command.
            for target_id in ["missing-retired-target", id] {
                for retired_value in [json!(live.stage_run_id()), Value::Null] {
                    let mut target = json!({"id": target_id, "nonce": live.nonce()});
                    target[key] = retired_value;
                    if with_new_key {
                        target["stage_run_id"] = json!(live.stage_run_id());
                    }
                    send_chat(
                        &mut write,
                        json!({"op": "chat", "text": "must not be delivered", "target": target}),
                    )
                    .await;
                    let response = frame(&mut reader).await;
                    assert_eq!(response["type"], "error", "{response}");
                    assert_eq!(
                        response["message"],
                        format!("target.{key} is retired; use target.stage_run_id"),
                        "{response}"
                    );
                    assert!(!execution.accept_pause(), "retired target acquired a pause");
                    assert!(!execution.is_attached());
                    let mut command = std::pin::pin!(execution.next_command());
                    std::future::poll_fn(|cx| {
                        assert!(
                            !matches!(command.as_mut().poll(cx), Poll::Ready(Some(_))),
                            "unexpected delivery"
                        );
                        Poll::Ready(())
                    })
                    .await;
                    assert_no_frame(&mut reader).await;
                    assert!(session.get_current_turn().await.is_none());
                    assert!(session.get_chat_handle().await.is_none());
                    assert!(session.get_chat_ids().await.is_empty());
                }
            }
        }
    }
    for key in ["stage_id", "execution_id", "stage_run_id"] {
        for with_target in [false, true] {
            for value in [json!(live.stage_run_id()), Value::Null] {
                let mut request = json!({"op": "chat", "text": "must not start a turn"});
                request[key] = value;
                if with_target {
                    request["target"] = json!({
                        "id": id, "nonce": live.nonce(),
                        "stage_run_id": live.stage_run_id()
                    });
                }
                send_chat(&mut write, request).await;
                let response = frame(&mut reader).await;
                assert_eq!(response["type"], "error", "{response}");
                assert_eq!(
                    response["message"],
                    "top-level stage-run selectors are not supported; use target with id, nonce, and stage_run_id"
                );
                assert!(!execution.accept_pause());
                assert!(!execution.is_attached());
                assert!(!execution.execution_ended());
                assert_no_frame(&mut reader).await;
                assert!(session.get_current_turn().await.is_none());
                assert!(session.get_chat_handle().await.is_none());
                assert!(session.get_chat_ids().await.is_empty());
            }
        }
    }
    assert!(!execution.execution_ended());
    assert!(registry.contains_run(id).await);
    drop(execution);
    registry.stop_run(id).await.unwrap();
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
        ("id/missing", json!({"nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("id/null", json!({"id": null, "nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("id/bool", json!({"id": false, "nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("id/number", json!({"id": 1, "nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("id/array", json!({"id": [], "nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("id/object", json!({"id": {}, "nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("id/empty", json!({"id": "", "nonce": 1, "stage_run_id": "execution"}), "target.id is required"),
        ("nonce/missing", json!({"id": "missing-run", "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/null", json!({"id": "missing-run", "nonce": null, "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/bool", json!({"id": "missing-run", "nonce": false, "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/string", json!({"id": "missing-run", "nonce": "1", "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/zero", json!({"id": "missing-run", "nonce": 0, "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/negative", json!({"id": "missing-run", "nonce": -1, "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/fraction", json!({"id": "missing-run", "nonce": 1.5, "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/array", json!({"id": "missing-run", "nonce": [], "stage_run_id": "execution"}), "target.nonce is required"),
        ("nonce/object", json!({"id": "missing-run", "nonce": {}, "stage_run_id": "execution"}), "target.nonce is required"),
        ("stage_run_id/missing", json!({"id": "missing-run", "nonce": 1}), "target.stage_run_id is required"),
        ("stage_run_id/null", json!({"id": "missing-run", "nonce": 1, "stage_run_id": null}), "target.stage_run_id is required"),
        ("stage_run_id/bool", json!({"id": "missing-run", "nonce": 1, "stage_run_id": false}), "target.stage_run_id is required"),
        ("stage_run_id/number", json!({"id": "missing-run", "nonce": 1, "stage_run_id": 1}), "target.stage_run_id is required"),
        ("stage_run_id/array", json!({"id": "missing-run", "nonce": 1, "stage_run_id": []}), "target.stage_run_id is required"),
        ("stage_run_id/object", json!({"id": "missing-run", "nonce": 1, "stage_run_id": {}}), "target.stage_run_id is required"),
        ("stage_run_id/empty", json!({"id": "missing-run", "nonce": 1, "stage_run_id": ""}), "target.stage_run_id is required"),
        ("another session owns the chat", json!({"id": id, "nonce": 1, "stage_run_id": "execution"}), "not owned by this session"),
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

// The debugger descriptor supplies compatibility identity only. Corrections use
// an independent execution inbox, without a readiness handshake or debug lease.
struct CorrectionPipeline {
    gremlin: crate::executor::gremlin::Gremlin,
    execution: crate::clients::interactive::InteractiveSession,
    target: crate::clients::interactive::DebugTarget,
    receiver: crate::clients::execution_input::InputReceiver,
    revoker: std::sync::Arc<crate::clients::execution_input::InputRevoker>,
}

async fn correction_pipeline(
    registry: &TestRegistry,
    project: &std::path::Path,
    id: &str,
) -> CorrectionPipeline {
    use crate::clients::cancel::CancelToken;
    use crate::clients::execution_input::ExecutionIdentity;

    let gremlin = create_chat_gremlin(
        id,
        &model("cmd/echo"),
        "helpful",
        project,
        &Default::default(),
    )
    .unwrap();
    gremlin
        .state
        .patch(&[], json!({"kind": "pipeline"}).as_object().unwrap());
    let mut gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    let execution = gremlin.next_interactive_session();
    let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
    let target = interactive.target();
    let (receiver, revoker) = gremlin.execution_inputs.register_with_nonce(
        ExecutionIdentity {
            run_id: id.into(),
            stage_name: "build".into(),
            stage_run_id: target.stage_run_id().unwrap().into(),
        },
        Some(target.nonce()),
        |_| {},
    );
    CorrectionPipeline {
        gremlin,
        execution,
        target,
        receiver,
        revoker,
    }
}

fn pipeline_target(id: &str, pipeline: &CorrectionPipeline) -> Value {
    json!({"id": id, "nonce": pipeline.target.nonce(),
        "stage_run_id": pipeline.target.stage_run_id()})
}

fn assert_correction_frame(response: &Value, target: &Value, req_id: &str, kind: &str) {
    assert_eq!(response["type"], kind, "{response}");
    assert_eq!(response["id"], target["id"], "{response}");
    if kind != "correction_dropped" {
        assert_eq!(response["nonce"], target["nonce"], "{response}");
    }
    assert_eq!(
        response["stage_run_id"], target["stage_run_id"],
        "{response}"
    );
    assert_eq!(response["req_id"], req_id, "{response}");
    assert_new_identity_only(response);
}

async fn receive_correction(
    receiver: &mut crate::clients::execution_input::InputReceiver,
) -> crate::clients::execution_input::Correction {
    let command = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await
        .expect("correction was not enqueued")
        .expect("execution input closed");
    let crate::clients::execution_input::InputCommand::Correction(correction) = command else {
        panic!("expected correction, got {command:?}");
    };
    correction
}

#[tokio::test]
async fn aliased_corrections_echo_client_identity_but_claim_canonical_execution() {
    use crate::clients::execution_input::ExecutionIdentity;

    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let id = "pipeline-aliased-correction";
    let mut pipeline = correction_pipeline(&registry, &project, id).await;
    let target = pipeline_target(id, &pipeline);
    pipeline.revoker.revoke();
    let canonical = ExecutionIdentity {
        run_id: id.into(),
        stage_name: "build".into(),
        stage_run_id: "canonical-invocation".into(),
    };
    let alias = ExecutionIdentity {
        stage_run_id: target["stage_run_id"].as_str().unwrap().into(),
        ..canonical.clone()
    };
    assert_ne!(canonical.stage_run_id, alias.stage_run_id);
    let (receiver, revoker) = pipeline.gremlin.execution_inputs.register_with_aliases(
        canonical.clone(),
        [alias],
        Some(pipeline.target.nonce()),
        |_| {},
    );
    pipeline.receiver = receiver;
    pipeline.revoker = revoker;

    let bound = super::super::input::bind(&target, &registry, Some(&session))
        .await
        .unwrap();
    assert_eq!(bound.identity, canonical);
    assert!(registry
        .claim_controller(id, &canonical.stage_run_id)
        .is_err());
    drop(bound);

    for req_id in ["delivered-alias", "dropped-alias"] {
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "Alias revision",
                "req_id": req_id, "target": target}),
        )
        .await;
        let ack = frame(&mut reader).await;
        assert_correction_frame(&ack, &target, req_id, "ok");
        assert_eq!(ack["status"], "queued");
        let correction = receive_correction(&mut pipeline.receiver).await;
        let claim = registry
            .claim_controller(id, &canonical.stage_run_id)
            .expect("queued correction retained canonical claim");
        drop(claim);
        if req_id == "delivered-alias" {
            assert!(correction.incorporate(|request_id, text| {
                assert_eq!(request_id, req_id);
                assert_eq!(text, "Alias revision");
            }));
            assert_correction_frame(&frame(&mut reader).await, &target, req_id, "delivered");
        } else {
            pipeline.revoker.revoke();
            assert_correction_frame(
                &frame(&mut reader).await,
                &target,
                req_id,
                "correction_dropped",
            );
            assert!(!correction.incorporate(|_, _| panic!("revoked correction applied")));
        }
    }
    assert_no_frame(&mut reader).await;
    send_chat(&mut write, json!({"op": "close_session"})).await;
    assert_eq!(frame(&mut reader).await["type"], "ok");
    task.await.unwrap();
    registry.stop_run(id).await.unwrap();
}

#[tokio::test]
async fn held_correction_releases_claim_and_keeps_main_dispatcher_responsive() {
    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let id = "pipeline-held-correction";
    let mut pipeline = correction_pipeline(&registry, &project, id).await;
    let target = pipeline_target(id, &pipeline);
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "First revision",
        "req_id": "first", "target": target}),
    )
    .await;
    let ack = frame(&mut reader).await;
    assert_correction_frame(&ack, &target, "first", "ok");
    assert_eq!(ack["status"], "queued");
    let first = receive_correction(&mut pipeline.receiver).await;
    assert_eq!(first.request_id, "first");
    assert_eq!(first.text, "First revision");

    // A held receipt must not retain the exclusive input controller claim.
    let bound = super::super::input::bind(&target, &registry, Some(&session))
        .await
        .expect("held correction retained controller claim");
    assert_eq!(
        bound.identity.stage_run_id,
        target["stage_run_id"].as_str().unwrap()
    );
    drop(bound);
    send_chat(
        &mut write,
        json!({"op": "chat", "text": "Second revision",
        "req_id": "second", "target": target}),
    )
    .await;
    let ack = frame(&mut reader).await;
    assert_correction_frame(&ack, &target, "second", "ok");
    assert_eq!(ack["status"], "queued");
    let second = receive_correction(&mut pipeline.receiver).await;
    assert!(session.get_current_turn().await.is_none());
    assert!(session.get_chat_handle().await.is_none());
    assert!(session.get_chat_ids().await.is_empty());
    assert!(!pipeline.execution.accept_pause());
    assert!(!pipeline.execution.is_attached());
    assert_no_frame(&mut reader).await;

    // Complete out of order: each receipt retains its own framing identity.
    assert!(second.incorporate(|req_id, text| {
        assert_eq!(req_id, "second");
        assert_eq!(text, "Second revision");
    }));
    assert_correction_frame(&frame(&mut reader).await, &target, "second", "delivered");
    assert!(first.incorporate(|req_id, text| {
        assert_eq!(req_id, "first");
        assert_eq!(text, "First revision");
    }));
    assert_correction_frame(&frame(&mut reader).await, &target, "first", "delivered");
    send_chat(&mut write, json!({"op": "close_session"})).await;
    assert_eq!(frame(&mut reader).await["type"], "ok");
    task.await.unwrap();
    registry.stop_run(id).await.unwrap();
}

async fn revoked_correction_does_not_retarget(polled: bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        use crate::clients::execution_input::ExecutionIdentity;

        let (_sandbox, project, _) = sandbox_dirs(None);
        let (registry, _) = spawn_test_actor().await;
        let (mut reader, mut write, _session, task) = main_socket(&registry).await;
        let id = "pipeline-revoked-correction";
        let mut pipeline = correction_pipeline(&registry, &project, id).await;
        let target = pipeline_target(id, &pipeline);
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "Old revision",
            "req_id": "revoked", "target": target}),
        )
        .await;
        let ack = frame(&mut reader).await;
        assert_correction_frame(&ack, &target, "revoked", "ok");
        assert_eq!(ack["status"], "queued");
        let held = if polled {
            Some(receive_correction(&mut pipeline.receiver).await)
        } else {
            assert!(pipeline.receiver.pending());
            None
        };
        pipeline.revoker.revoke();
        drop(pipeline.execution);
        let mut successor = pipeline.gremlin.next_interactive_session();
        let (interactive, _, _) = registry.get_run_interactive(id).await.unwrap();
        let next_target = interactive.target();
        assert_ne!(next_target.stage_run_id(), pipeline.target.stage_run_id());
        let (mut next_receiver, _next_revoker) =
            pipeline.gremlin.execution_inputs.register_with_nonce(
                ExecutionIdentity {
                    run_id: id.into(),
                    stage_name: "build".into(),
                    stage_run_id: next_target.stage_run_id().unwrap().into(),
                },
                Some(next_target.nonce()),
                |_| {},
            );
        let dropped = frame(&mut reader).await;
        assert_correction_frame(&dropped, &target, "revoked", "correction_dropped");
        assert_eq!(dropped["reason"], "agent exited before incorporation");
        if let Some(held) = held {
            assert!(!held.incorporate(|_, _| panic!("revoked correction was applied")));
        }
        assert!(pipeline.receiver.recv().await.is_none());
        for _ in 0..128 {
            tokio::task::yield_now().await;
            assert!(matches!(
                next_receiver.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            assert!(!successor.accept_pause());
            assert!(!successor.is_attached());
        }
        assert_no_frame(&mut reader).await;
        send_chat(&mut write, json!({"op": "close_session"})).await;
        assert_eq!(frame(&mut reader).await["type"], "ok");
        task.await.unwrap();
        registry.stop_run(id).await.unwrap();
    })
    .await
    .expect("revocation must settle without consumer progress");
}

#[tokio::test]
async fn revocation_drops_queued_unconsumed_correction_without_successor_leakage() {
    revoked_correction_does_not_retarget(false).await;
}

#[tokio::test]
async fn revocation_drops_polled_held_correction_without_successor_leakage() {
    revoked_correction_does_not_retarget(true).await;
}

#[tokio::test]
async fn full_execution_inbox_rejects_correction_without_queued_ack() {
    let (_sandbox, project, _) = sandbox_dirs(None);
    let (registry, _) = spawn_test_actor().await;
    let (mut reader, mut write, session, task) = main_socket(&registry).await;
    let id = "pipeline-full-correction";
    let pipeline = correction_pipeline(&registry, &project, id).await;
    let target = pipeline_target(id, &pipeline);
    let bound = super::super::input::bind(&target, &registry, Some(&session))
        .await
        .unwrap();
    let mut receipts = Vec::new();
    for index in 0..crate::clients::execution_input::INPUT_CAPACITY {
        receipts.push(
            bound
                .sender
                .try_send(format!("fill-{index}"), "fill".into())
                .unwrap(),
        );
    }
    drop(bound);
    send_chat(
        &mut write,
        json!({"op":"chat", "text":"overflow",
        "req_id":"overflow", "target":target}),
    )
    .await;
    let rejected = frame(&mut reader).await;
    assert_eq!(rejected["type"], "error", "{rejected}");
    assert!(rejected.to_string().contains("full"), "{rejected}");
    assert_no_frame(&mut reader).await;
    let bound = super::super::input::bind(&target, &registry, Some(&session))
        .await
        .expect("failed enqueue retained controller claim");
    drop(bound);
    assert_no_frame(&mut reader).await;
    send_chat(&mut write, json!({"op":"close_session"})).await;
    assert_eq!(frame(&mut reader).await["type"], "ok");
    task.await.unwrap();
    registry.stop_run(id).await.unwrap();
}

#[tokio::test]
async fn close_session_with_held_correction_leaves_independent_target_bindable() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_sandbox, project, _) = sandbox_dirs(None);
        let (registry, _) = spawn_test_actor().await;
        let (mut reader, mut write, session, task) = main_socket(&registry).await;
        let id = "pipeline-close-correction";
        let mut pipeline = correction_pipeline(&registry, &project, id).await;
        let target = pipeline_target(id, &pipeline);
        let end = pipeline.target.end_signal();
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "Held revision",
            "req_id": "held", "target": target}),
        )
        .await;
        let ack = frame(&mut reader).await;
        assert_correction_frame(&ack, &target, "held", "ok");
        assert_eq!(ack["status"], "queued");
        let held = receive_correction(&mut pipeline.receiver).await;
        assert!(session.get_current_turn().await.is_none());
        assert!(session.get_chat_handle().await.is_none());
        assert!(session.get_chat_ids().await.is_empty());
        assert_no_frame(&mut reader).await;

        // Closing this unrelated MAIN session must not wait for incorporation.
        // Keep both the socket and the correction alive through the ack and exit.
        send_chat(&mut write, json!({"op": "close_session"})).await;
        assert_eq!(frame(&mut reader).await["type"], "ok");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("close_session waited for correction incorporation")
            .unwrap();
        assert!(!end.is_ended(), "MAIN close ended independent execution");
        assert!(registry.contains_run(id).await);
        let bound = super::super::input::bind(&target, &registry, Some(&session))
            .await
            .expect("MAIN close retained input controller claim");
        drop(bound);
        assert!(!pipeline.execution.accept_pause());
        assert!(!pipeline.execution.is_attached());

        drop(held);
        pipeline.revoker.revoke();
        registry.stop_run(id).await.unwrap();
    })
    .await
    .expect("close_session must respond while incorporation is withheld");
}

#[tokio::test]
async fn main_socket_eof_with_held_correction_leaves_independent_target_bindable() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_sandbox, project, _) = sandbox_dirs(None);
        let (registry, _) = spawn_test_actor().await;
        let (mut reader, mut write, session, task) = main_socket(&registry).await;
        let id = "pipeline-eof-correction";
        let mut pipeline = correction_pipeline(&registry, &project, id).await;
        let target = pipeline_target(id, &pipeline);
        let end = pipeline.target.end_signal();
        send_chat(
            &mut write,
            json!({"op": "chat", "text": "Held revision",
            "req_id": "held", "target": target}),
        )
        .await;
        let ack = frame(&mut reader).await;
        assert_correction_frame(&ack, &target, "held", "ok");
        assert_eq!(ack["status"], "queued");
        let held = receive_correction(&mut pipeline.receiver).await;
        drop(write);
        task.await.unwrap();
        assert!(!end.is_ended(), "MAIN teardown ended independent execution");
        assert!(registry.contains_run(id).await);
        let bound = super::super::input::bind(&target, &registry, Some(&session))
            .await
            .expect("MAIN EOF retained input controller claim");
        drop(bound);
        assert!(!pipeline.execution.accept_pause());
        assert!(!pipeline.execution.is_attached());
        // A spawned receipt waiter may survive EOF. Do not require delivery to
        // have been invalidated; only MAIN exit and independent input matter.
        drop(held);
        pipeline.revoker.revoke();
        registry.stop_run(id).await.unwrap();
    })
    .await
    .expect("MAIN EOF must not wait for incorporation");
}
