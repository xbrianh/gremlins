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
use crate::executor::supervisor::{attachment, handle_session_main};
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
    .expect("a paused turn must end without debugger input");
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

    let (debug, server) = UnixStream::pair().unwrap();
    let debug_registry = registry.clone();
    let debug_session = session.clone();
    let debug_server = tokio::spawn(async move {
        let (read, write) = server.into_split();
        let mut reader = BufReader::new(read);
        let request = read_json_line(&mut reader).await.unwrap().unwrap();
        attachment::handle_attach_owned(
            &request,
            &debug_registry,
            Some(&debug_session),
            reader,
            &SharedWriter::new(write),
        )
        .await;
    });
    let (read, mut debug_write) = debug.into_split();
    let mut debug = BufReader::new(read);
    write_json_line(
        &mut debug_write,
        &json!({"op": "attach", "scope": "conversation", "id": id, "mode": "debugger"}),
    )
    .await
    .unwrap();
    let attached = read_until_type(&mut debug, "attached").await;
    assert_eq!(attached["id"], id);
    assert_eq!(read_until_type(&mut debug, "ready").await["id"], id);

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
        let ended = read_until_type(&mut debug, "ended").await;
        assert_eq!(ended["reason"], "interrupted");
        drop(debug_write);
        drop(debug);
        timeout(WAIT, debug_server).await.unwrap().unwrap();
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
    // Neither half of the debug connection has been dropped or sent a command.
    let reason = if mode == "hard" {
        "cancelled"
    } else {
        "interrupted"
    };
    expect_ack(&mut chat).await;
    expect_cancellation(&mut events, id, reason).await;
    expect_quiet(&mut chat).await;
    // Dropping the command receiver isn't a terminal event. The debugger
    // must see the same classified outcome as the bus, not just socket EOF.
    let ended = read_until_type(&mut debug, "ended").await;
    assert_eq!(ended["type"], "ended");
    assert_eq!(ended["scope"], "conversation");
    assert_eq!(ended["reason"], reason);
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
    drop(debug_write);
    drop(debug);
    timeout(WAIT, debug_server).await.unwrap().unwrap();

    let _held_followup = if mode == "permanent" {
        None
    } else {
        write_json_line(&mut write, &prompt("next", FOLLOWUP))
            .await
            .unwrap();
        assert_eq!(expect_ack(&mut chat).await["id"], id);
        let (request, held) = pending_request(&provider).await;
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
async fn lost_prompt_reader_does_not_backpressure_and_classifies_debugger_cancellation() {
    cancel_paused_chat("soft", true).await;
}
