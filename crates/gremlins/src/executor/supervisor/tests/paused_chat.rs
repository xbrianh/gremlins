use std::time::Duration;

use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{subscribe_run, EventKind};
use serde_json::json;
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{TcpListener, UnixStream};
use tokio::time::timeout;

use super::{create_session, read_until_type, spawn_test_actor};
use crate::executor::socket::{read_json_line, write_json_line};
use crate::executor::supervisor::test_provider::pending_request;
use crate::executor::supervisor::{handle_connection, handle_session_main};
use crate::test_support::Sandbox;

const FIRST: &str = "Review my first draft.";
const FOLLOWUP: &str = "Review my revision.";
const NOTICE: &str = "Your previous response was interrupted.";
const WAIT: Duration = Duration::from_secs(10);

async fn expect_cancellation(chat: &mut BufReader<OwnedReadHalf>, reason: &str) {
    // The cancel ack and the turn's terminal event may arrive in either order.
    timeout(WAIT, async {
        let (mut ack, mut ended) = (false, false);
        while !ack || !ended {
            let frame = read_json_line(chat).await.unwrap().expect("chat closed");
            match frame["type"].as_str() {
                Some("ok") => ack = true,
                Some("ended") => {
                    assert_eq!(frame["chat_turn"], "initial");
                    assert_eq!(frame["reason"], reason);
                    ended = true;
                }
                Some("done" | "turn_complete" | "error") => {
                    panic!("unexpected chat event: {frame}")
                }
                _ => (),
            }
        }
    })
    .await
    .expect("a paused turn must end without debugger input");
}

async fn cancel_paused_chat(mode: &str) {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key": "test", "base-url": format!("http://{}", provider.local_addr().unwrap()),
        }})
        .to_string(),
    );
    std::fs::write(
        sandbox.join("config/settings.yaml"),
        format!("default-model: openrouter/test-paused-{mode}\n"),
    )
    .unwrap();
    let (registry, _) = spawn_test_actor().await;
    let (session_id, session) = create_session(&registry).await;
    let (client, server) = UnixStream::pair().unwrap();
    let (server_read, server_write) = server.into_split();
    let chat_server = tokio::spawn(handle_session_main(
        server_read,
        SharedWriter::new(server_write),
        session_id.clone(),
        registry.clone(),
        session.clone(),
    ));
    let (read, mut write) = client.into_split();
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
    let ack = read_until_type(&mut chat, "ok").await;
    let id = ack["id"].as_str().unwrap();
    let (first, _held) = pending_request(&provider).await;
    assert!(first["messages"].to_string().contains(FIRST), "{first}");

    let (debug, server) = UnixStream::pair().unwrap();
    let debug_server = tokio::spawn(handle_connection(
        server,
        registry.clone(),
        Default::default(),
    ));
    let (read, mut debug_write) = debug.into_split();
    let mut debug = BufReader::new(read);
    write_json_line(
        &mut debug_write,
        &json!({"op": "attach", "scope": "conversation", "id": id}),
    )
    .await
    .unwrap();
    // New protocol: attach acks without pausing. Send `continue` to acquire
    // the debugger lease and pause the agent.
    let attached = read_until_type(&mut debug, "attached").await;
    assert_eq!(attached["id"], id);
    write_json_line(&mut debug_write, &json!({"op": "continue"}))
        .await
        .unwrap();
    assert_eq!(read_until_type(&mut debug, "ready").await["id"], id);
    // The lease's `continue` runs one turn, which re-issues the interrupted
    // request. Hold it so the follow-up assertion below sees the next
    // request (the actual follow-up prompt) rather than this re-run.
    let (_rerun, _rerun_held) = pending_request(&provider).await;

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
    expect_cancellation(&mut chat, reason).await;
    // Dropping the command receiver isn't a terminal event. The debugger
    // must see the same classified outcome as the chat, not just socket EOF.
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
        assert_eq!(read_until_type(&mut chat, "ok").await["id"], id);
        let (request, held) = pending_request(&provider).await;
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
    cancel_paused_chat("soft").await;
}

#[tokio::test]
async fn hard_cancel_paused_chat_discards_interrupted_turn() {
    cancel_paused_chat("hard").await;
}

#[tokio::test]
async fn permanent_stop_exits_paused_chat_task() {
    cancel_paused_chat("permanent").await;
}
