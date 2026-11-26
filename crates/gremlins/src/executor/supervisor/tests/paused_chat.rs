use std::time::Duration;

use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run, EventKind, ReceiverHalf, Recv};
use serde_json::json;
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{TcpListener, UnixStream};
use tokio::time::timeout;

use super::{create_session, read_until_type, spawn_test_actor};
use crate::executor::socket::{read_json_line, write_json_line};
use crate::executor::supervisor::test_provider::pending_request;
use crate::executor::supervisor::{handle_session_main, input};
use crate::test_support::Sandbox;

const FIRST: &str = "Review my first draft.";
const FOLLOWUP: &str = "Review my revision.";
const NOTICE: &str = "Your previous response was interrupted.";
const WAIT: Duration = Duration::from_secs(10);

async fn expect_cancellation(events: &mut ReceiverHalf<EventKind>, id: &str, reason: &str) {
    timeout(WAIT, async {
        loop {
            let Recv::Event(event) = events.recv().await else {
                panic!("bus closed or lagged");
            };
            match event.kind() {
                EventKind::Ended { reason: actual } => {
                    assert_eq!(event.corr_id().run(), Some(id));
                    assert!(event.corr_id().to_string().contains("initial"));
                    assert_eq!(actual, reason);
                    break;
                }
                EventKind::Done { .. }
                | EventKind::Error { .. }
                | EventKind::PromptRequest { .. } => {
                    panic!("unexpected cancellation event: {:?}", event.kind());
                }
                _ => (),
            }
        }
    })
    .await
    .expect("a paused turn must end without further input");
}

async fn expect_ack(chat: &mut BufReader<OwnedReadHalf>) -> serde_json::Value {
    let ack = timeout(WAIT, read_json_line(chat))
        .await
        .expect("request acknowledgement timed out")
        .unwrap()
        .expect("chat closed");
    assert_eq!(ack["type"], "ok", "{ack}");
    ack
}

async fn expect_quiet(chat: &mut BufReader<OwnedReadHalf>) {
    assert!(
        timeout(Duration::from_millis(50), read_json_line(chat))
            .await
            .is_err(),
        "prompt socket received unsolicited output or closed"
    );
}

async fn cancel_paused_chat(mode: &str, lose_chat_writer: bool) {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key": "test", "base-url": format!("http://{}", provider.local_addr().unwrap()),
        }})
        .to_string(),
    );
    std::fs::write(
        sandbox.join("config/settings.yaml"),
        format!("default-model: openrouter/test-paused-{mode}-{lose_chat_writer}\n"),
    )
    .unwrap();
    let (registry, _) = spawn_test_actor().await;
    let (session_id, session) = create_session(&registry).await;
    let (client, server) = UnixStream::pair().unwrap();
    // Separate request and response sockets so losing only the chat writer
    // does not trigger session teardown (which would race the test cancel).
    let (response_client, response_server) = UnixStream::pair().unwrap();
    let (server_read, _unused) = server.into_split();
    let (_, server_write) = response_server.into_split();
    let chat_server = tokio::spawn(handle_session_main(
        server_read,
        SharedWriter::new(server_write),
        session_id.clone(),
        registry.clone(),
        session.clone(),
    ));
    let (_, mut write) = client.into_split();
    let (read, _) = response_client.into_split();
    let mut chat = BufReader::new(read);
    let prompt = |turn: &str, text: &str| {
        json!({
            "op": "chat", "session_id": session_id, "turn": turn, "text": text,
            "project_root": sandbox.path(),
        })
    };
    write_json_line(&mut write, &prompt("initial", FIRST))
        .await
        .unwrap();
    let ack = expect_ack(&mut chat).await;
    assert!(ack.get("stage_run_id").is_none(), "{ack}");
    assert!(ack.get("nonce").is_none(), "{ack}");
    let id = ack["id"].as_str().unwrap();
    let (first, _held) = pending_request(&provider).await;
    assert!(first["messages"].to_string().contains(FIRST), "{first}");

    let current = session.get_current_turn().await.expect("active turn");
    let execution = current.stage_run_id.expect("exact execution identity");
    let target = json!({"id": id, "stage_run_id": execution});
    let (control, server) = UnixStream::pair().unwrap();
    let input_registry = registry.clone();
    let input_session = session.clone();
    let input_server = tokio::spawn(async move {
        let (read, write) = server.into_split();
        let mut reader = BufReader::new(read);
        let request = read_json_line(&mut reader).await.unwrap().unwrap();
        input::handle_input(
            &request,
            &input_registry,
            Some(&input_session),
            reader,
            &SharedWriter::new(write),
        )
        .await;
    });
    let (read, mut control_write) = control.into_split();
    let mut control = BufReader::new(read);
    write_json_line(
        &mut control_write,
        &json!({"op": "input", "target": target, "req_id": "bind"}),
    )
    .await
    .unwrap();
    let bound = read_until_type(&mut control, "input_bound").await;
    assert_eq!(bound["id"], id);
    assert_eq!(bound["stage_run_id"], execution);
    write_json_line(
        &mut control_write,
        &json!({"cmd": "pause", "req_id": "pause"}),
    )
    .await
    .unwrap();
    assert_eq!(
        read_until_type(&mut control, "queued").await["req_id"],
        "pause"
    );
    assert_eq!(
        read_until_type(&mut control, "delivered").await["req_id"],
        "pause"
    );

    if lose_chat_writer {
        drop(chat);
        // Losing the ack-only prompt reader must not backpressure the stage.
        // More events than the bounded mailbox can hold prove the runner
        // keeps draining bus output. Keep the model request pending until then.
        timeout(WAIT, async {
            for _ in 0..258 {
                gremlins_magic_bus::emit_run_event_async(id, EventKind::StreamReset).await;
            }
        })
        .await
        .expect("lost prompt reader kept backpressuring the stage");
        let (sub, mut events) = subscribe_run(id);
        let _guard = BusSubGuard::new(sub);
        session.cancel_turn(Some(id)).await.expect("active turn");
        expect_cancellation(&mut events, id, "interrupted").await;
        read_until_type(&mut control, "ended").await;
        drop(control_write);
        drop(control);
        timeout(WAIT, input_server).await.unwrap().unwrap();
        drop(write);
        timeout(WAIT, chat_server).await.unwrap().unwrap();
        return;
    }

    let (sub, mut events) = subscribe_run(id);
    let _guard = BusSubGuard::new(sub);
    let handle = session.get_chat_handle().await.unwrap();
    write_json_line(
        &mut write,
        &json!({
            "op": "cancel_chat", "session_id": session_id, "turn": "initial", "mode": mode
        }),
    )
    .await
    .unwrap();
    // Keep the paused controller connected; cancellation must wake the runner.
    let reason = if mode == "hard" {
        "cancelled"
    } else {
        "interrupted"
    };
    expect_ack(&mut chat).await;
    expect_cancellation(&mut events, id, reason).await;
    expect_quiet(&mut chat).await;
    // Input revocation is separate from the classified bus outcome.
    read_until_type(&mut control, "ended").await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.kind(), EventKind::PromptRequest { .. }),
            "cancel started another model request"
        );
    }

    if mode == "permanent" {
        timeout(WAIT, handle.prompt_tx.closed())
            .await
            .expect("permanent stop must exit the chat task");
        assert!(handle.is_finished());
    }
    drop(control_write);
    drop(control);
    timeout(WAIT, input_server).await.unwrap().unwrap();

    let _held_followup = if mode == "permanent" {
        None
    } else {
        write_json_line(&mut write, &prompt("next", FOLLOWUP))
            .await
            .unwrap();
        assert_eq!(expect_ack(&mut chat).await["id"], id);
        let (request, held) = pending_request(&provider).await;
        let restarted = session.get_current_turn().await.expect("restarted turn");
        assert_ne!(restarted.stage_run_id.as_deref(), Some(execution.as_str()));
        let stale = json!({"op": "input", "target": target, "req_id": "stale"});
        let error = match input::bind(&stale, &registry, Some(&session)).await {
            Ok(_) => panic!("old execution accepted input after restart"),
            Err(error) => error,
        };
        assert!(error.contains("stale"), "{error}");
        expect_quiet(&mut chat).await;
        let messages = request["messages"].to_string();
        assert!(messages.contains(FOLLOWUP), "{messages}");
        assert_eq!(messages.contains(FIRST), mode == "soft", "{messages}");
        assert_eq!(messages.contains(NOTICE), mode == "soft", "{messages}");
        Some(held)
    };
    // EOF on the chat session cancels any held follow-up and reaps its run.
    drop(write);
    timeout(WAIT, chat_server).await.unwrap().unwrap();
}

#[tokio::test]
async fn review_input_disconnect_keeps_real_agent_paused_until_cancelled() {
    use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop, test_ctx};
    use crate::clients::error::ClientError;
    use crate::clients::execution_input::{ExecutionIdentity, InputRegistry};
    use crate::executor::supervisor::{handle_connection, CancelToken};
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent as Event};
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let id = "paused-review-wire";
    let identity = ExecutionIdentity {
        run_id: id.into(),
        stage_name: "review".into(),
        stage_run_id: "known-review-execution".into(),
    };
    let gremlin = super::create_chat_gremlin(
        id,
        &super::model("cmd/echo"),
        "helpful",
        dir.path(),
        &Default::default(),
    )
    .unwrap();
    gremlin
        .state
        .patch(&[], json!({"kind": "pipeline"}).as_object().unwrap());
    let (registry, _) = spawn_test_actor().await;
    let mut gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    let inputs = InputRegistry::default();
    let (handle, _, _) = registry.get_run_interactive(id).await.unwrap();
    handle.set_input_registry(inputs.clone());
    let cancel = CancelToken::new();
    let mut execution = gremlin.next_interactive_session();
    execution.bind_input(&inputs, identity.clone(), true, Some(&cancel));

    // Only exact input: no watch, bus-derived selector, or session ownership.
    let (mut control, server) = UnixStream::pair().unwrap();
    let input_server = tokio::spawn(handle_connection(
        server,
        registry.clone(),
        Default::default(),
    ));
    write_json_line(
        &mut control,
        &json!({"op": "input", "id": id, "stage": "review",
            "stage_run_id": identity.stage_run_id, "req_id": "bind"}),
    )
    .await
    .unwrap();
    let (control_read, mut control_write) = control.into_split();
    let mut control = BufReader::new(control_read);
    let bound = read_until_type(&mut control, "input_bound").await;
    assert_eq!(bound["id"], id);
    assert_eq!(bound["stage"], "review");
    assert_eq!(bound["stage_run_id"], identity.stage_run_id);
    write_json_line(
        &mut control_write,
        &json!({"cmd": "pause", "req_id": "pause"}),
    )
    .await
    .unwrap();
    assert_eq!(
        read_until_type(&mut control, "queued").await["req_id"],
        "pause"
    );

    let model = MockCompletionModel::from_stream_turns([
        vec![
            Event::tool_call(
                "progress",
                "Write",
                json!({
                    "file_path": "review-progress.txt", "content": "review advanced"
                }),
            ),
            Event::final_response_with_default_usage(),
        ],
        vec![
            Event::tool_call("done", "Done", json!({"summary": "review complete"})),
            Event::final_response_with_default_usage(),
        ],
    ]);
    let mut agent = tokio::spawn(run_agent_loop(
        model.clone().erase(),
        "Write review-progress.txt, then finish the review.",
        test_ctx(Some(dir.path().to_owned()), None),
        cancel.clone(),
        loop_opts(None),
        execution,
    ));
    // Only the real agent's inbox boundary can apply this pause and settle it.
    assert_eq!(
        read_until_type(&mut control, "delivered").await["req_id"],
        "pause"
    );
    assert!(model.requests().is_empty());
    assert!(!dir.path().join("review-progress.txt").exists());

    control_write.shutdown().await.unwrap();
    timeout(WAIT, input_server).await.unwrap().unwrap();
    drop(control);
    // A ready model would write and finish if EOF synthesized resume. A live
    // inbox plus no requests rules out termination masquerading as a pause.
    assert!(handle.input_sender(&identity).is_some());
    assert!(timeout(Duration::from_millis(100), &mut agent)
        .await
        .is_err());
    assert!(model.requests().is_empty(), "disconnect resumed the review");
    assert!(!dir.path().join("review-progress.txt").exists());

    cancel.cancel();
    let result = timeout(WAIT, agent)
        .await
        .expect("paused review must terminate on cancellation without resume")
        .unwrap();
    assert!(matches!(result, Err(ClientError::Cancelled)), "{result:?}");
    assert!(
        model.requests().is_empty(),
        "cancellation resumed the review"
    );
    assert!(!dir.path().join("review-progress.txt").exists());
    assert!(handle.input_sender(&identity).is_none());
    registry.stop_run(id).await.unwrap();
}

#[tokio::test]
async fn soft_cancel_paused_chat_preserves_interruption_history() {
    cancel_paused_chat("soft", false).await;
}

#[tokio::test]
async fn hard_cancel_paused_chat_discards_interrupted_turn() {
    cancel_paused_chat("hard", false).await;
}

#[tokio::test]
async fn permanent_stop_exits_paused_chat_task() {
    cancel_paused_chat("permanent", false).await;
}

#[tokio::test]
async fn lost_prompt_reader_does_not_backpressure_and_classifies_paused_cancellation() {
    cancel_paused_chat("soft", true).await;
}
