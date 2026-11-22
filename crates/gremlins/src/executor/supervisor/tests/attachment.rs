//! Tests for `handle_conversation_attach`: the shared conversation-scope
//! attach handler. Covers leaf tracking, disconnect before/after pause
//! acknowledgment, command-queue backpressure, and re-attachment.
//!
//! These tests call `handle_conversation_attach` directly on `UnixStream::pair`
//! so they exercise the full handshake, command loop, and bus subscription path
//! without going through the socket-routing layer.
//!
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
    let request = json!({"op": "attach", "scope": "conversation", "id": id, "mode": "debugger"});
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
    connection_mode(registry, id, Some("debugger"))
}

fn connection_mode(
    registry: &Arc<RunRegistry>,
    id: &str,
    mode: Option<&str>,
) -> (BufReader<UnixStream>, impl Future<Output = ()> + 'static) {
    let (client, server) = UnixStream::pair().unwrap();
    let registry = registry.clone();
    let mut request = json!({"op": "attach", "scope": "conversation", "id": id});
    if let Some(mode) = mode {
        request["mode"] = json!(mode);
    }
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

/// Next frame that carries an outcome: skips progress (`status`), display
/// (`ready`) and acknowledgment (`attached`) frames.
async fn next_response(reader: &mut BufReader<UnixStream>) -> Value {
    loop {
        let msg = socket::read_json_line(reader).await.unwrap().unwrap();
        if msg["type"] != "status"
            && msg["type"] != "attached"
            && msg["type"] != "ready"
            && msg["type"] != "execution_ready"
            && msg["type"] != "operator_message"
        {
            return msg;
        }
    }
}

/// Read frames until one satisfies `matches`, ignoring display events.
async fn wait_for(client: &mut BufReader<UnixStream>, matches: impl Fn(&Value) -> bool) -> Value {
    loop {
        let frame = socket::read_json_line(client)
            .await
            .unwrap()
            .expect("attach connection ended");
        assert_ne!(frame["type"], "error", "{frame}");
        if matches(&frame) {
            return frame;
        }
    }
}

/// Detach is a subsequent command on the same socket and an EOF fence for
/// duplicate confirmations after the first matching delivery was consumed.
async fn detach_after_delivery(
    client: &mut BufReader<UnixStream>,
    handler: impl Future<Output = ()>,
    req_id: &str,
) {
    socket::write_json_line(client.get_mut(), &json!({"op":"detach"}))
        .await
        .unwrap();
    handler.await;
    let mut deliveries = 1;
    let mut detached = 0;
    while let Some(frame) = socket::read_json_line(client).await.unwrap() {
        assert_ne!(frame["type"], "error", "{frame}");
        if frame["type"] == "delivered" {
            assert_eq!(frame["req_id"], req_id, "{frame}");
            deliveries += 1;
        }
        if frame["type"] == "detached" {
            assert_eq!(frame["scope"], "conversation", "{frame}");
            detached += 1;
        }
    }
    assert_eq!(deliveries, 1, "duplicate delivered confirmation");
    assert_eq!(detached, 1, "subsequent detach command did not succeed");
}

/// A synthetic live execution: no provider or agent task can race the test.
async fn synthetic_run(id: &str) -> (Arc<RunRegistry>, Gremlin, tempfile::TempDir) {
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
    let gremlin = registry
        .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
        .await;
    gremlin.state.set_stage("chat-0001", None, "");
    (registry, gremlin, dir)
}

/// POSIX FIFO handshake: opening the writer proves the child reached its
/// blocking read. No shell-specific networking or timing window is needed.
struct ChildGate {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl ChildGate {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gate");
        assert!(std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        Self { _dir: dir, path }
    }

    fn command(&self) -> String {
        format!("read -r go < \"{}\"", self.path.display())
    }

    async fn entered(&self) -> tokio::fs::File {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .await
            .unwrap()
    }
}

async fn pending_once(future: impl Future) {
    tokio::pin!(future);
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "completed before incorporation"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn observed_readiness_is_not_control_readiness() {
    timeout(Duration::from_secs(5), async {
        for mode in [Some("passive"), Some("control")] {
            let id = format!("observed-ready-{}", mode.unwrap());
            let (registry, mut gremlin, _dir) = synthetic_run(&id).await;
            let mut session = gremlin.next_interactive_session();
            let (mut observer, handler) = connection_mode(&registry, &id, mode);
            let task = tokio::spawn(handler);
            let ack = wait_for(&mut observer, |f| f["type"] == "attached").await;
            // A competing owner, not this observer, acknowledged readiness.
            let (handle, _, _) = registry.get_run_interactive(&id).await.unwrap();
            let lease = handle.target().attach().expect("observer reserved control");
            assert!(session.accept_pause());
            gremlins_magic_bus::emit_corr_event(
                &gremlins_magic_bus::CorrId::new()
                    .with_run(&id)
                    .with_execution_id(ack["execution_id"].as_str().unwrap()),
                EventKind::Ready { turn: 7 },
            );
            let frame = loop {
                let frame = socket::read_json_line(&mut observer)
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(
                    frame["type"], "ready",
                    "observer advertised command readiness: {frame}"
                );
                if frame["type"] == "execution_ready" {
                    break frame;
                }
            };
            assert_eq!(frame["turn"], 7);
            drop(lease);
            drop(observer);
            task.await.unwrap();
            registry.stop_run(&id).await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn incorporation_releases_temporary_lease_while_display_is_blocked() {
    timeout(Duration::from_secs(5), async {
        let id = "incorporation-blocked-display";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let mut session = gremlin.next_interactive_session();
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        client.set_nonblocking(true).unwrap();
        let mut filler = server.try_clone().unwrap();
        let (read, write) = UnixStream::from_std(server).unwrap().into_split();
        let writer = SharedWriter::new(write);
        let mut client = BufReader::new(UnixStream::from_std(client).unwrap());
        let request = json!({"op":"attach", "scope":"conversation", "id":id});
        let handler =
            handle_conversation_attach(&request, id, &registry, BufReader::new(read), &writer);
        tokio::pin!(handler);
        let ack = tokio::select! {
            _ = &mut handler => panic!("handler ended"),
            frame = wait_for(&mut client, |f| f["type"] == "attached") => frame,
        };
        socket::write_json_line(
            client.get_mut(),
            &json!({
                "op":"message", "text":"incorporate", "req_id":"blocked-display"
            }),
        )
        .await
        .unwrap();
        tokio::select! {
            _ = &mut handler => panic!("handler ended"),
            _ = wait_for(&mut client, |f| f["stage"] == "waiting_for_ready") => {},
        }
        assert!(session.accept_pause());
        let command = tokio::select! {
            _ = &mut handler => panic!("handler ended"),
            command = session.next_command() => command.unwrap(),
        };
        let InteractiveCommand::Message(_, _, Some(delivered)) = command else {
            panic!("missing incorporation sender");
        };
        // Finish pre-delivery readiness forwarding before applying pressure.
        pending_once(&mut handler).await;
        fill_attach_socket(&mut filler);
        drop(filler);
        let (entered, release) =
            crate::executor::supervisor::attachment::delivery_display_gate::install(id);
        emit_text(id, ack["execution_id"].as_str().unwrap(), "blocked display");
        tokio::select! {
            _ = &mut handler => panic!("handler ended before display"),
            result = entered => result.unwrap(),
        }
        release.send(()).unwrap();
        pending_once(&mut handler).await;
        session.finish_message_delivery();
        delivered.send(()).unwrap();
        // Do not drain the socket: neither display nor confirmation can flush.
        pending_once(&mut handler).await;
        let (handle, _, _) = registry.get_run_interactive(id).await.unwrap();
        let replacement = handle
            .target()
            .attach()
            .expect("blocked display retained temporary lease");
        assert!(session.accept_pause());
        let frame = tokio::select! {
            _ = &mut handler => panic!("handler ended"),
            frame = wait_for(&mut client, |f| f["type"] == "delivered") => frame,
        };
        assert_eq!(frame["req_id"], "blocked-display");
        socket::write_json_line(client.get_mut(), &json!({"op":"detach"}))
            .await
            .unwrap();
        let (_, ()) = tokio::join!(&mut handler, async {
            wait_for(&mut client, |f| f["type"] == "detached").await;
        });
        assert!(
            session.is_attached(),
            "old cleanup cleared replacement lease"
        );
        drop(replacement);
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn default_observe_attach_does_not_pause() {
    timeout(Duration::from_secs(5), async {
        let id = "observe-no-pause";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let mut session = gremlin.next_interactive_session();
        let (mut client, handler) = connection_mode(&registry, id, None);
        tokio::pin!(handler);
        tokio::select! {
            _ = &mut handler => panic!("observer ended"),
            _ = wait_for(&mut client, |f| f["type"] == "attached") => {}
        }
        // Poll through all post-ack work, not a wall-clock sleep.
        pending_once(&mut handler).await;
        assert!(!session.accept_pause(), "default attach acquired a pause");
        drop(client);
        handler.await;
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn immediate_message_waits_for_incorporation_and_releases_only_temporary_lease() {
    timeout(Duration::from_secs(5), async {
        let id = "observe-message";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let mut session = gremlin.next_interactive_session();
        let (mut client, handler) = connection_mode(&registry, id, None);
        tokio::pin!(handler);
        // Pipeline the message before even reading the attached frame.
        socket::write_json_line(client.get_mut(), &json!({
            "op":"message", "text":"new plan", "req_id":"correction-17"
        })).await.unwrap();
        let ack = tokio::select! {
            _ = &mut handler => panic!("handler ended"),
            ack = socket::read_json_line(&mut client) => ack.unwrap().unwrap(),
        };
        assert_eq!(ack["type"], "attached");
        tokio::select! {
            _ = &mut handler => panic!("handler ended before readiness"),
            _ = wait_for(&mut client, |f| f["type"] == "status" && f["stage"] == "waiting_for_ready") => {}
        }
        assert!(session.accept_pause());
        let command = tokio::select! {
            _ = &mut handler => panic!("handler ended before command"),
            command = session.next_command() => command.unwrap(),
        };
        let InteractiveCommand::Message(text, req_id, Some(delivered)) = command else {
            panic!("expected message with incorporation signal");
        };
        assert_eq!(text, "new plan");
        assert_eq!(req_id.as_deref(), Some("correction-17"));
        pending_once(&mut handler).await;
        // Drain readiness/display frames. No outcome may precede incorporation.
        loop {
            let read = socket::read_json_line(&mut client);
            tokio::pin!(read);
            let frame = std::future::poll_fn(|cx| match read.as_mut().poll(cx) {
                std::task::Poll::Ready(value) => std::task::Poll::Ready(Some(value.unwrap().unwrap())),
                std::task::Poll::Pending => std::task::Poll::Ready(None),
            }).await;
            let Some(frame) = frame else { break };
            assert!(matches!(frame["type"].as_str(), Some("ready" | "execution_ready" | "status" | "operator_message")), "premature outcome: {frame}");
        }
        session.finish_message_delivery();
        delivered.send(()).unwrap();
        let response = tokio::select! {
            _ = &mut handler => panic!("temporary message closed socket"),
            frame = next_response(&mut client) => frame,
        };
        assert_eq!(response["type"], "delivered", "{response}");
        assert_eq!(response["req_id"], "correction-17");
        assert_eq!(response["id"], id);
        assert_eq!(response["nonce"], ack["nonce"]);
        assert_eq!(response["execution_id"], ack["execution_id"]);
        assert!(!session.is_attached());
        // A new owner can acquire the same execution while the socket remains open.
        let (handle, _, _) = registry.get_run_interactive(id).await.unwrap();
        let replacement = handle.target().attach().expect("temporary lease leaked");
        detach_after_delivery(&mut client, &mut handler, "correction-17").await;
        assert!(session.accept_pause(), "old socket cleanup cleared replacement");
        drop(replacement);
        registry.stop_run(id).await.unwrap();
    }).await.unwrap();
}

/// Incorporation and execution end are both ready before the handler resumes,
/// in its delivery select, display forwarding, or a busy reply. Confirmation
/// must win without requiring another live-session poll.
async fn confirmed_message_survives_execution_end(
    mode: Option<&str>,
    during_display: bool,
    during_busy: bool,
) {
    let id = format!(
        "confirmed-before-end-{}-display-{during_display}-busy-{during_busy}",
        mode.unwrap_or("observe")
    );
    let req_id = "confirmed-original-request";
    let (registry, mut gremlin, _dir) = synthetic_run(&id).await;
    let mut session = gremlin.next_interactive_session();
    let (mut client, handler) = connection_mode(&registry, &id, mode);
    tokio::pin!(handler);
    let message = json!({
        "op":"message", "text":"incorporated before end", "req_id":req_id
    });
    if mode.is_none() {
        socket::write_json_line(client.get_mut(), &message)
            .await
            .unwrap();
    }
    let ack = tokio::select! {
        _ = &mut handler => panic!("handler ended before attach"),
        frame = socket::read_json_line(&mut client) => frame.unwrap().unwrap(),
    };
    assert_eq!(ack["type"], "attached");
    assert!(ack["nonce"].is_u64(), "{ack}");
    assert!(ack["execution_id"].is_string(), "{ack}");
    tokio::select! {
        _ = &mut handler => panic!("handler ended before readiness"),
        _ = wait_for(&mut client, |f| f["type"] == "status" && f["stage"] == "waiting_for_ready") => {}
    }
    assert!(session.accept_pause());
    let command = tokio::select! {
        _ = &mut handler => panic!("handler ended before command"),
        result = async {
            let (command, ()) = tokio::join!(session.next_command(), async {
                if mode.is_some() {
                    wait_for(&mut client, |f| f["type"] == "ready").await;
                    socket::write_json_line(client.get_mut(), &message).await.unwrap();
                }
            });
            command.unwrap()
        } => result,
    };
    let InteractiveCommand::Message(text, received_req_id, Some(delivered)) = command else {
        panic!("expected message with incorporation signal");
    };
    assert_eq!(text, "incorporated before end");
    assert_eq!(received_req_id.as_deref(), Some(req_id));
    pending_once(&mut handler).await;

    let gated_text = "display held across confirmation and execution end";
    let release = if during_display {
        let (entered, release) =
            crate::executor::supervisor::attachment::delivery_display_gate::install(&id);
        emit_text(&id, ack["execution_id"].as_str().unwrap(), gated_text);
        tokio::select! {
            _ = &mut handler => panic!("handler ended before delivery display gate"),
            result = entered => result.unwrap(),
        }
        Some(release)
    } else if during_busy {
        let (entered, release) =
            crate::executor::supervisor::attachment::delivery_busy_gate::install(&id);
        socket::write_json_line(
            client.get_mut(),
            &json!({"op":"message", "text":"second correction", "req_id":"second-request"}),
        )
        .await
        .unwrap();
        tokio::select! {
            _ = &mut handler => panic!("handler ended before delivery busy gate"),
            result = entered => result.unwrap(),
        }
        Some(release)
    } else {
        None
    };

    // Do not poll the handler between confirming incorporation and ending A.
    session.finish_message_delivery();
    delivered.send(()).unwrap();
    drop(session);
    if let Some(release) = release {
        release.send(()).unwrap();
    }

    let (_, ()) = tokio::join!(&mut handler, async {
        let mut deliveries = 0;
        while let Some(frame) = socket::read_json_line(&mut client).await.unwrap() {
            assert_ne!(frame["type"], "error", "{frame}");
            assert_ne!(frame["type"], "correction_dropped", "{frame}");
            assert!(
                frame["type"] != "stream_chunk" || frame["text"] != gated_text,
                "forwarded display after confirmation and execution end: {frame}"
            );
            if frame["type"] == "delivered" {
                deliveries += 1;
                assert_eq!(frame["req_id"], req_id, "{frame}");
                assert_eq!(frame["id"], id, "{frame}");
                assert_eq!(frame["nonce"], ack["nonce"], "{frame}");
                assert_eq!(frame["execution_id"], ack["execution_id"], "{frame}");
            }
        }
        assert_eq!(deliveries, 1, "expected exactly one delivery before EOF");
    });
    registry.stop_run(&id).await.unwrap();
}

#[tokio::test]
async fn confirmed_temporary_message_survives_execution_end_before_handler_poll() {
    timeout(
        Duration::from_secs(5),
        confirmed_message_survives_execution_end(None, false, false),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn confirmed_debugger_message_survives_execution_end_before_handler_poll() {
    timeout(
        Duration::from_secs(5),
        confirmed_message_survives_execution_end(Some("debugger"), false, false),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn confirmed_temporary_message_survives_execution_end_during_display() {
    timeout(
        Duration::from_secs(5),
        confirmed_message_survives_execution_end(None, true, false),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn confirmed_debugger_message_survives_execution_end_during_display() {
    timeout(
        Duration::from_secs(5),
        confirmed_message_survives_execution_end(Some("debugger"), true, false),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn confirmed_temporary_message_survives_execution_end_during_busy_reply() {
    timeout(
        Duration::from_secs(5),
        confirmed_message_survives_execution_end(None, false, true),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn confirmed_debugger_message_survives_execution_end_during_busy_reply() {
    timeout(
        Duration::from_secs(5),
        confirmed_message_survives_execution_end(Some("debugger"), false, true),
    )
    .await
    .unwrap();
}

/// Fill the actual kernel send buffer, not a writer mutex or a timed mock.
fn fill_attach_socket(socket: &mut std::os::unix::net::UnixStream) {
    use std::io::{ErrorKind, Write};

    let spaces = [b' '; 4096];
    let mut written = 0;
    loop {
        match socket.write(&spaces) {
            Ok(0) => panic!("attach socket closed while filling"),
            Ok(n) => written += n,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill attach socket: {error}"),
        }
    }
    assert!(written > 0);
    assert_eq!(
        socket.write(b" ").unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
}

async fn successful_delivery_under_backpressure(queue_message: bool) {
    timeout(Duration::from_secs(10), async {
        let id = format!("delivery-backpressure-{queue_message}");
        let (registry, mut gremlin, _dir) = synthetic_run(&id).await;
        let mut session = gremlin.next_interactive_session();
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        client.set_nonblocking(true).unwrap();
        let mut filler = server.try_clone().unwrap();
        let (read, write) = UnixStream::from_std(server).unwrap().into_split();
        let writer = SharedWriter::new(write);
        let mut client = BufReader::new(UnixStream::from_std(client).unwrap());
        let request = json!({"op":"attach", "scope":"conversation", "id":id});
        let handler = handle_conversation_attach(
            &request, &id, &registry, BufReader::new(read), &writer,
        );
        tokio::pin!(handler);
        let ack = tokio::select! {
            _ = &mut handler => panic!("attachment ended"),
            ack = wait_for(&mut client, |f| f["type"] == "attached") => ack,
        };
        socket::write_json_line(client.get_mut(), &json!({
            "op":"message", "text":"first correction", "req_id":"first"
        })).await.unwrap();
        tokio::select! {
            _ = &mut handler => panic!("attachment ended before readiness"),
            _ = wait_for(&mut client, |f| f["stage"] == "waiting_for_ready") => {}
        }
        assert!(session.accept_pause());
        let command = tokio::select! {
            _ = &mut handler => panic!("attachment ended before correction"),
            command = session.next_command() => command.unwrap(),
        };
        let InteractiveCommand::Message(_, _, Some(delivered)) = command else {
            panic!("expected correction with incorporation signal");
        };
        fill_attach_socket(&mut filler);
        drop(filler);
        session.finish_message_delivery();
        delivered.send(()).unwrap();
        pending_once(&mut handler).await;
        assert!(!session.is_attached(), "successful correction retained its lease");
        let (handle, _, _) = registry.get_run_interactive(&id).await.unwrap();
        let replacement = handle.target().attach().expect("temporary lease leaked under backpressure");
        drop(replacement);

        if queue_message {
            socket::write_json_line(client.get_mut(), &json!({
                "op":"message", "text":"second correction", "req_id":"second"
            })).await.unwrap();
        }
        // Keep A live and the socket unread/full beyond the old flush deadline.
        tokio::select! {
            _ = &mut handler => panic!("successful delivery closed a live attachment under backpressure"),
            _ = tokio::time::sleep(Duration::from_millis(1200)) => {}
        }
        assert!(!session.is_attached());
        let first = tokio::select! {
            _ = &mut handler => panic!("attachment ended while draining delivery"),
            frame = wait_for(&mut client, |f| f["type"] == "delivered") => frame,
        };
        assert_eq!(first["req_id"], "first");
        for key in ["id", "nonce", "execution_id"] {
            assert_eq!(first[key], ack[key], "delivery changed {key}");
        }
        if queue_message {
            tokio::select! {
                _ = &mut handler => panic!("queued command closed attachment"),
                _ = wait_for(&mut client, |f| f["stage"] == "waiting_for_ready") => {}
            }
            assert!(session.accept_pause(), "queued correction was swallowed");
            let command = tokio::select! {
                _ = &mut handler => panic!("queued correction was lost"),
                command = session.next_command() => command.unwrap(),
            };
            let InteractiveCommand::Message(text, req_id, Some(delivered)) = command else {
                panic!("expected queued correction");
            };
            assert_eq!(text, "second correction");
            assert_eq!(req_id.as_deref(), Some("second"));
            session.finish_message_delivery();
            delivered.send(()).unwrap();
            tokio::select! {
                _ = &mut handler => panic!("second delivery closed attachment"),
                _ = wait_for(&mut client, |f| f["type"] == "delivered" && f["req_id"] == "second") => {}
            }
        }
        assert!(!session.is_attached());
        emit_text(&id, ack["execution_id"].as_str().unwrap(), "still viewing A");
        tokio::select! {
            _ = &mut handler => panic!("attachment did not resume viewing A"),
            _ = wait_for(&mut client, |f| f["text"] == "still viewing A") => {}
        }
        socket::write_json_line(client.get_mut(), &json!({"op":"detach"})).await.unwrap();
        handler.await;
        let detached = socket::read_json_line(&mut client).await.unwrap().unwrap();
        assert_eq!(detached["type"], "detached", "{detached}");
        registry.stop_run(&id).await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn successful_correction_survives_socket_backpressure_past_one_second() {
    successful_delivery_under_backpressure(false).await;
}

#[tokio::test]
async fn successful_correction_preserves_command_queued_behind_blocked_delivery() {
    successful_delivery_under_backpressure(true).await;
}

async fn message_interrupted(accepted: bool, end_execution: bool) {
    let id = format!("message-interrupted-{accepted}-{end_execution}");
    let (registry, mut gremlin, _dir) = synthetic_run(&id).await;
    let mut session = gremlin.next_interactive_session();
    let (mut client, handler) = connection_mode(&registry, &id, None);
    let handler = tokio::spawn(handler);
    let ack = socket::read_json_line(&mut client).await.unwrap().unwrap();
    assert_eq!(ack["type"], "attached");
    socket::write_json_line(
        client.get_mut(),
        &json!({
            "op":"message", "text":"never incorporated", "req_id":"interrupted"
        }),
    )
    .await
    .unwrap();
    wait_for(&mut client, |f| {
        f["type"] == "status" && f["stage"] == "waiting_for_ready"
    })
    .await;
    let delivery = if accepted {
        assert!(session.accept_pause());
        let command = session.next_command().await.unwrap();
        let InteractiveCommand::Message(_, _, Some(delivery)) = command else {
            panic!("missing incorporation signal");
        };
        Some(delivery)
    } else {
        None
    };
    if end_execution {
        drop(session);
        // End must cancel both readiness and incorporation without a bus event.
        let mut dropped = 0;
        loop {
            let frame = socket::read_json_line(&mut client).await.unwrap();
            let Some(frame) = frame else { break };
            assert_ne!(frame["type"], "delivered", "{frame}");
            assert_ne!(frame["type"], "error", "{frame}");
            if frame["type"] == "correction_dropped" {
                dropped += 1;
                assert_eq!(frame["scope"], "conversation");
                assert_eq!(frame["id"], id);
                assert_eq!(frame["nonce"], ack["nonce"]);
                assert_eq!(frame["execution_id"], ack["execution_id"]);
                assert_eq!(frame["req_id"], "interrupted");
            }
        }
        assert_eq!(dropped, 1, "missing or duplicate correction outcome");
        handler.await.unwrap();
        let mut successor = gremlin.next_interactive_session();
        assert!(!successor.accept_pause(), "message reserved a successor");
    } else {
        drop(client);
        handler.await.unwrap();
        let (handle, _, _) = registry.get_run_interactive(&id).await.unwrap();
        let replacement = handle.target().attach().expect("EOF leaked the lease");
        drop(replacement);
    }
    drop(delivery);
    registry.stop_run(&id).await.unwrap();
}

#[tokio::test]
async fn message_eof_releases_readiness_and_incorporation_waits() {
    timeout(Duration::from_secs(5), async {
        message_interrupted(false, false).await;
        message_interrupted(true, false).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn message_execution_end_cancels_readiness_and_incorporation_waits() {
    timeout(Duration::from_secs(5), async {
        message_interrupted(false, true).await;
        message_interrupted(true, true).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn observe_nonmessage_acquires_retained_live_target() {
    timeout(Duration::from_secs(5), async {
        let id = "observe-retained-command";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let mut session = gremlin.next_interactive_session();
        let (mut client, handler) = connection_mode(&registry, id, None);
        let handler = tokio::spawn(handler);
        wait_for(&mut client, |f| f["type"] == "attached").await;
        socket::write_json_line(client.get_mut(), &json!({"op":"continue"})).await.unwrap();
        wait_for(&mut client, |f| f["type"] == "status" && f["stage"] == "waiting_for_ready").await;
        assert!(session.accept_pause());
        assert!(matches!(session.next_command().await, Some(InteractiveCommand::RunTurn)));
        // Later commands use this same retained acquisition, not a fresh lease.
        socket::write_json_line(client.get_mut(), &json!({"op":"talk", "text":"same owner"})).await.unwrap();
        assert!(matches!(session.next_command().await, Some(InteractiveCommand::Inject(text)) if text == "same owner"));
        assert!(session.has_accepted_acquisition());
        drop(client);
        handler.await.unwrap();
        registry.stop_run(id).await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn retained_debugger_message_keeps_ownership_until_replaced() {
    timeout(Duration::from_secs(5), async {
        let id = "retained-message-owner";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let mut session = gremlin.next_interactive_session();
        let (mut first, handler) = connection(&registry, id);
        let handler = tokio::spawn(handler);
        wait_for(&mut first, |f| {
            f["type"] == "status" && f["stage"] == "waiting_for_ready"
        })
        .await;
        assert!(session.accept_pause());
        let (command, ()) = tokio::join!(session.next_command(), async {
            wait_for(&mut first, |f| f["type"] == "ready").await;
            socket::write_json_line(
                first.get_mut(),
                &json!({
                    "op":"message", "text":"retained", "req_id":"retained-1"
                }),
            )
            .await
            .unwrap();
        });
        let Some(InteractiveCommand::Message(_, _, Some(delivery))) = command else {
            panic!("missing retained delivery signal");
        };
        session.finish_message_delivery();
        delivery.send(()).unwrap();
        let delivered = next_response(&mut first).await;
        assert_eq!(delivered["type"], "delivered", "{delivered}");
        assert_eq!(delivered["req_id"], "retained-1");
        assert!(
            session.has_accepted_acquisition(),
            "message released debugger ownership"
        );
        socket::write_json_line(first.get_mut(), &json!({"op":"continue"}))
            .await
            .unwrap();
        assert!(matches!(
            session.next_command().await,
            Some(InteractiveCommand::RunTurn)
        ));

        // A live debugger cannot be stolen, even after message delivery.
        let (mut contender, contender_handler) = connection(&registry, id);
        let contender_handler = tokio::spawn(contender_handler);
        let busy = next_response(&mut contender).await;
        assert_eq!(busy["type"], "error", "{busy}");
        assert_eq!(busy["message"], "busy", "{busy}");
        drop(contender);
        contender_handler.await.unwrap();

        drop(first);
        handler.await.unwrap();

        // Only after disconnect can a replacement request its own pause.
        let (mut second, replacement) = connection(&registry, id);
        let replacement = tokio::spawn(replacement);
        wait_for(&mut second, |f| {
            f["type"] == "status" && f["stage"] == "waiting_for_ready"
        })
        .await;
        session.detach_revoked_debugger();
        assert!(
            session.accept_pause(),
            "old owner cleared replacement pause"
        );
        let (command, ()) = tokio::join!(session.next_command(), async {
            wait_for(&mut second, |f| f["type"] == "ready").await;
            socket::write_json_line(second.get_mut(), &json!({"op":"continue"}))
                .await
                .unwrap();
        });
        assert!(matches!(command, Some(InteractiveCommand::RunTurn)));
        drop(second);
        replacement.await.unwrap();
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

/// Exercise reservation withdrawal on either side of minting, and after ready.
async fn reserved_debugger_disconnect(handoff: bool, accepted: bool) {
    let id = format!("reserved-{handoff}-{accepted}");
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key":"test", "base-url":format!("http://{}", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    let gremlin = create_chat_gremlin(
        &id,
        &model(&format!("openrouter/{id}")),
        "helpful",
        sandbox.path(),
        &Default::default(),
    )
    .unwrap();
    let cancel = CancelToken::new();
    let (registry, _) = spawn_test_actor().await;
    let mut gremlin = registry
        .register_chat_gremlin(id.clone(), gremlin, cancel.clone())
        .await;
    let (mut owner, handler) = connection(&registry, &id);
    let handler = tokio::spawn(handler);
    wait_for(&mut owner, |f| {
        f["type"] == "status" && f["stage"] == "waiting_for_ready"
    })
    .await;
    let mut session = handoff.then(|| gremlin.next_interactive_session());
    let mut owner = Some(owner);
    let mut handler = Some(handler);
    if !accepted {
        drop(owner.take());
        handler.take().unwrap().await.unwrap();
    }
    let mut session = session
        .take()
        .unwrap_or_else(|| gremlin.next_interactive_session());
    if !accepted {
        assert!(
            !session.accept_pause(),
            "{id}: disconnected reservation still pending"
        );
    }
    gremlin.state.set_stage("chat-0001", None, "");
    let agent = gremlin.client.run(
        RunParams {
            prompt: "Proceed autonomously.".into(),
            cwd: Some(sandbox.path().to_path_buf()),
            gremlin_id: Some(id.clone()),
            cancel_token: Some(cancel.clone()),
            ..RunParams::empty()
        },
        session,
    );
    tokio::pin!(agent);
    if accepted {
        tokio::select! {
            result = &mut agent => panic!("agent stopped: {result:?}"),
            _ = wait_for(owner.as_mut().unwrap(), |f| f["type"] == "ready") => {}
        }
        drop(owner.take());
        // Wait for socket cleanup before proving autonomous progress.
        handler.take().unwrap().await.unwrap();
    }
    // A held model request proves progress without a replacement or command.
    let (body, _held) = tokio::select! {
        result = &mut agent => panic!("agent stopped: {result:?}"),
        request = timeout(Duration::from_secs(3), pending_request(&provider)) => request.unwrap_or_else(|_| panic!("{id} did not resume")),
    };
    assert!(body.to_string().contains("Proceed autonomously."));
    cancel.cancel();
    assert!(matches!(agent.await, Err(ClientError::Cancelled)));
    registry.stop_run(&id).await.unwrap();
}

#[tokio::test]
async fn between_turn_debugger_withdrawal_resumes_without_commands() {
    timeout(Duration::from_secs(15), async {
        reserved_debugger_disconnect(false, false).await;
        reserved_debugger_disconnect(true, false).await;
        reserved_debugger_disconnect(true, true).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ready_debugger_rejects_competing_message_without_losing_ownership() {
    timeout(Duration::from_secs(5), async {
        let id = "ready-competitor";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let mut session = gremlin.next_interactive_session();
        let (mut owner, handler) = connection(&registry, id);
        let handler = tokio::spawn(handler);
        wait_for(&mut owner, |f| f["type"] == "status").await;
        assert!(session.accept_pause());
        tokio::select! {
            command = session.next_command() => panic!("unexpected command: {command:?}"),
            _ = wait_for(&mut owner, |f| f["type"] == "ready") => {}
        }
        let (mut competitor, competing_handler) = connection_mode(&registry, id, None);
        let competing_handler = tokio::spawn(competing_handler);
        socket::write_json_line(
            competitor.get_mut(),
            &json!({
                "op":"message", "text":"must not enter history", "req_id":"competitor"
            }),
        )
        .await
        .unwrap();
        let busy = next_response(&mut competitor).await;
        assert_eq!(busy["type"], "error", "{busy}");
        assert_eq!(busy["message"], "busy", "{busy}");
        drop(competitor);
        competing_handler.await.unwrap();
        assert!(session.has_accepted_acquisition());
        socket::write_json_line(owner.get_mut(), &json!({"op":"talk", "text":"still owner"}))
            .await
            .unwrap();
        assert!(matches!(session.next_command().await,
            Some(InteractiveCommand::Inject(text)) if text == "still owner"));
        drop(owner);
        handler.await.unwrap();
        assert!(session.next_command().await.is_none());
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn old_default_attachment_cannot_bind_same_nonce_replacement() {
    timeout(Duration::from_secs(5), async {
        let id = "old-default-replacement";
        let (registry, mut old, dir) = synthetic_run(id).await;
        let old_session = old.next_interactive_session();
        let (mut client, handler) = connection_mode(&registry, id, None);
        tokio::pin!(handler);
        let ack = tokio::select! {
            _ = &mut handler => panic!("observer ended"),
            frame = socket::read_json_line(&mut client) => frame.unwrap().unwrap(),
        };
        assert_eq!(ack["type"], "attached");
        // Keep the old handler unpolled through replacement. Its queued command
        // wins over the end signal, exercising the captured target itself.
        registry.stop_run(id).await.unwrap();
        drop(old_session);
        drop(old);
        let replacement = create_chat_gremlin(
            id,
            &model("cmd/echo"),
            "helpful",
            dir.path(),
            &Default::default(),
        )
        .unwrap();
        let mut replacement = registry
            .register_chat_gremlin(id.into(), replacement, CancelToken::new())
            .await;
        replacement.state.set_stage("chat-0001", None, "");
        let mut session = replacement.next_interactive_session();
        let (handle, _, _) = registry.get_run_interactive(id).await.unwrap();
        assert_eq!(ack["nonce"], handle.target().nonce());
        assert_ne!(ack["execution_id"], json!(handle.target().execution_id()));
        socket::write_json_line(
            client.get_mut(),
            &json!({"op":"message", "text":"stale correction"}),
        )
        .await
        .unwrap();
        // The captured end signal may close first; either closure or rejection
        // is valid, but no outcome may claim delivery to the replacement.
        handler.await;
        while let Some(frame) = socket::read_json_line(&mut client).await.unwrap() {
            assert_ne!(frame["type"], "delivered", "{frame}");
        }
        assert!(!session.accept_pause(), "old observer acquired replacement");
        let lease = handle
            .target()
            .attach()
            .expect("old socket reserved replacement");
        drop(client);
        assert!(session.accept_pause(), "old cleanup cleared new lease");
        drop(lease);
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn default_attach_during_live_request_only_pauses_for_immediate_correction() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key":"test", "base-url":format!("http://{}", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    timeout(Duration::from_secs(15), async {
        let id = "live-default-correction";
        let cancel = CancelToken::new();
        let gremlin = create_chat_gremlin(
            id,
            &model("openrouter/live-default"),
            "helpful",
            sandbox.path(),
            &Default::default(),
        )
        .unwrap();
        let (registry, _) = spawn_test_actor().await;
        let mut gremlin = registry
            .register_chat_gremlin(id.into(), gremlin, cancel.clone())
            .await;
        gremlin.state.set_stage("chat-0001", None, "");
        let session = gremlin.next_interactive_session();
        let agent = gremlin.client.run(
            RunParams {
                prompt: "Original task.".into(),
                gremlin_id: Some(id.into()),
                cancel_token: Some(cancel.clone()),
                ..RunParams::empty()
            },
            session,
        );
        tokio::pin!(agent);
        let (_, mut held) = tokio::select! {
            result = &mut agent => panic!("agent stopped: {result:?}"),
            request = pending_request(&provider) => request,
        };
        let (sub, mut events) = subscribe_run(id);
        let _guard = BusSubGuard::new(sub);
        let (mut client, handler) = connection_mode(&registry, id, None);
        tokio::pin!(handler);
        let ack = tokio::select! {
            _ = &mut handler => panic!("observer ended"),
            frame = socket::read_json_line(&mut client) => frame.unwrap().unwrap(),
        };
        assert_eq!(ack["type"], "attached");
        // The same socket that will correct the agent must first observe its
        // live partial response, without sending any pause/control command.
        held.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"held partial answer\"},\"finish_reason\":null}]}\n\n").await.unwrap();
        tokio::select! {
            result = &mut agent => panic!("agent stopped: {result:?}"),
            _ = &mut handler => panic!("observer ended before partial response"),
            _ = wait_for(&mut client, |f| f["type"] == "stream_chunk" && f["text"] == "held partial answer") => {}
        }
        pending_once(&mut handler).await;
        // Observation must not reserve control. Acquire and release a competing
        // lease without polling the agent, so it cannot acknowledge the pause.
        let (handle, _, _) = registry.get_run_interactive(id).await.unwrap();
        let competing = handle
            .target()
            .attach()
            .expect("default attach reserved control");
        drop(competing);
        pending_once(&mut agent).await;
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.kind(), EventKind::Ready { .. }),
                "default attach paused live agent"
            );
        }
        // The original HTTP stream is still open: observation did not interrupt it.
        use tokio::io::AsyncReadExt;
        let mut byte = [0];
        pending_once(held.read(&mut byte)).await;
        socket::write_json_line(
            client.get_mut(),
            &json!({
                "op":"message", "text":"use corrected approach", "req_id":"live-correction"
            }),
        )
        .await
        .unwrap();
        // Hold the real agent before it can observe the pause. The handler
        // cannot confirm incorporation just because the command was queued.
        tokio::select! {
            _ = &mut handler => panic!("handler ended before pause"),
            _ = wait_for(&mut client, |f| f["stage"] == "waiting_for_ready") => {}
        }
        pending_once(&mut handler).await;
        pending_once(socket::read_json_line(&mut client)).await;
        let response = tokio::select! {
            result = &mut agent => panic!("agent stopped: {result:?}"),
            _ = &mut handler => panic!("correction handler ended"),
            frame = next_response(&mut client) => frame,
        };
        assert_eq!(response["type"], "delivered", "{response}");
        assert_eq!(response["req_id"], "live-correction");
        assert_eq!(response["id"], id);
        assert_eq!(response["nonce"], ack["nonce"]);
        assert_eq!(response["execution_id"], ack["execution_id"]);
        // No continue/quit is sent. Temporary ownership must release itself,
        // and the very next model request must contain the incorporated text.
        let (corrected, mut corrected_stream) = tokio::select! {
            result = &mut agent => panic!("agent stopped: {result:?}"),
            _ = &mut handler => panic!("observer ended after correction"),
            request = pending_request(&provider) => request,
        };
        assert!(
            corrected
                .to_string()
                .contains("[operator]: use corrected approach"),
            "{corrected}"
        );
        let request_text = corrected.to_string();
        assert_eq!(request_text.matches("held partial answer").count(), 1, "{corrected}");
        assert_eq!(request_text.matches("[operator]: use corrected approach").count(), 1, "{corrected}");
        corrected_stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"corrected output continues\"},\"finish_reason\":null}]}\n\n").await.unwrap();
        tokio::select! {
            result = &mut agent => panic!("agent stopped: {result:?}"),
            _ = &mut handler => panic!("observer ended"),
            _ = wait_for(&mut client, |f| f["type"] == "stream_chunk" && f["text"] == "corrected output continues") => {}
        }
        detach_after_delivery(&mut client, &mut handler, "live-correction").await;
        cancel.cancel();
        assert!(matches!(agent.await, Err(ClientError::Cancelled)));
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

fn emit_text(id: &str, execution: &str, text: &str) {
    gremlins_magic_bus::emit_corr_event(
        &gremlins_magic_bus::CorrId::new()
            .with_run(id)
            .with_execution_id(execution),
        EventKind::StreamChunk { text: text.into() },
    );
}

/// Produce a real streaming response, optionally leaving the request in flight.
async fn stream_answer(stream: &mut tokio::net::TcpStream, text: &str, finish: bool) {
    let chunk = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\n").as_bytes()).await.unwrap();
    if finish {
        let done = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
            "index":0,"id":"done-a","type":"function",
            "function":{"name":"Done","arguments":"{\"summary\":\"A complete\"}"}
        }]},"finish_reason":"tool_calls"}]});
        stream
            .write_all(format!("data: {done}\n\ndata: [DONE]\n\n").as_bytes())
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn buffered_viewers_do_not_cross_same_name_replacement() {
    timeout(Duration::from_secs(20), async {
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sandbox = Sandbox::with_providers(&json!({"openrouter": {
            "api-key":"test", "base-url":format!("http://{}", provider.local_addr().unwrap())
        }}).to_string());
        for mode in [Some("passive"), None] {
            // Subscribe run-follow before either agent starts. Both consecutive
            // leaves deliberately have the same name, but distinct executions.
            let gate = ChildGate::new();
            let cmd = gate.command();
            let pipeline = json!({
                "default_model":"openrouter/replacement-test",
                "stages":[{"name":"pipeline", "type":"sequence", "body":[
                    {"name":"gate", "type":"exec", "options":{"cmds":[cmd]}},
                    {"name":"first", "type":"sequence", "body":[
                        {"name":"work", "type":"agent", "model":"openrouter/replacement-test", "prompt":["Produce A."]}
                    ]},
                    {"name":"second", "type":"sequence", "body":[
                        {"name":"work", "type":"agent", "model":"openrouter/replacement-test", "prompt":["Produce B."]}
                    ]}
                ]}]
            });
            let definition = StaticDefinition::from_expanded_bytes(
                &serde_json::to_vec(&pipeline).unwrap(), None, None,
            ).unwrap();
            let gremlin = Gremlin::init(
                "buffered-replacement", &sandbox.join("replacement.json5"),
                &definition, &Default::default(), None,
                &GremlinConfig { ephemeral:true, ..Default::default() },
            ).unwrap();
            let registry = Arc::new(RunRegistry::new(tokio::sync::watch::channel(false).0));
            let id = registry.launch_run(gremlin, "pipeline".into(), None, "sequence".into(), false).await;
            let mut gate_stream = gate.entered().await;
            let (follow_client, follow_server) = UnixStream::pair().unwrap();
            let follow_task = tokio::spawn(crate::executor::supervisor::handle_connection(
                follow_server, registry.clone(), Default::default(),
            ));
            let mut follow = BufReader::new(follow_client);
            socket::write_json_line(follow.get_mut(), &json!({
                "op":"attach", "scope":"run", "id":id, "follow":true
            })).await.unwrap();
            wait_for(&mut follow, |f| f["type"] == "attached").await;
            gate_stream.write_all(b"go\n").await.unwrap();
            let (a_request, mut a_stream) = pending_request(&provider).await;
            assert!(a_request.to_string().contains("Produce A."), "{a_request}");
            let (entered, release) =
                crate::executor::supervisor::attachment::receiver_gate::install(&id);
            let (mut old, handler) = connection_mode(&registry, &id, mode);
            let old_task = tokio::spawn(handler);
            entered.await.unwrap();
            let a_ack = socket::read_json_line(&mut old).await.unwrap().unwrap();
            assert_eq!(a_ack["type"], "attached");
            stream_answer(&mut a_stream, "A buffered", true).await;
            let (b_request, mut b_stream) = pending_request(&provider).await;
            assert!(b_request.to_string().contains("Produce B."), "{b_request}");
            // End boundaries currently carry an empty stage. Ignore enclosing
            // sequence boundaries, but require A-start, A-end, B-start in order.
            wait_for(&mut follow, |f| f["type"] == "stage_boundary" && f["stage"] == "work" && f["active"] == true).await;
            let ended = wait_for(&mut follow, |f| f["type"] == "stage_boundary").await;
            assert_eq!(ended["active"], false, "{ended}");
            wait_for(&mut follow, |f| f["type"] == "stage_boundary" && f["stage"] == "work" && f["active"] == true).await;
            let (mut fresh, handler) = connection_mode(&registry, &id, mode);
            let fresh_task = tokio::spawn(handler);
            let b_ack = socket::read_json_line(&mut fresh).await.unwrap().unwrap();
            assert_eq!(b_ack["type"], "attached");
            assert_ne!(a_ack["execution_id"], b_ack["execution_id"]);
            assert_ne!(a_ack["nonce"], b_ack["nonce"]);
            stream_answer(&mut b_stream, "B live", false).await;
            wait_for(&mut fresh, |f| f["type"] == "stream_chunk" && f["text"] == "B live").await;
            // B has really produced output while A's receiver is still gated.
            release.send(()).unwrap();
            old_task.await.unwrap();
            while let Some(frame) = socket::read_json_line(&mut old).await.unwrap() {
                assert_ne!(frame["text"], "B live", "{frame}");
                assert_ne!(frame["type"], "delivered", "{frame}");
                if frame["type"] == "stream_chunk" {
                    assert_eq!(frame["text"], "A buffered", "{frame}");
                }
            }
            registry.stop_run(&id).await.unwrap();
            fresh_task.await.unwrap();
            follow_task.abort();
            drop(follow);
            let _ = follow_task.await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn debugger_live_target_expiring_during_setup_does_not_reserve_successor() {
    timeout(Duration::from_secs(5), async {
        let id = "debugger-ended-during-setup";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let a = gremlin.next_interactive_session();
        let (entered, release) = crate::executor::supervisor::attachment::setup_gate::install(id);
        let (mut client, handler) = connection(&registry, id);
        tokio::pin!(handler);
        tokio::select! {
            _ = &mut handler => panic!("debugger ended before setup gate"),
            result = entered => result.unwrap(),
        }
        drop(a);
        release.send(()).unwrap();
        handler.await;
        while let Some(frame) = socket::read_json_line(&mut client).await.unwrap() {
            assert_ne!(frame["type"], "ready", "{frame}");
        }
        let mut b = gremlin.next_interactive_session();
        assert!(!b.accept_pause(), "A's debugger reserved B during setup");
        drop(b);
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn debugger_live_target_ending_before_lease_does_not_reserve_successor() {
    timeout(Duration::from_secs(5), async {
        let id = "debugger-ended-before-lease";
        let (registry, mut gremlin, _dir) = synthetic_run(id).await;
        let a = gremlin.next_interactive_session();
        let (entered, release) =
            crate::executor::supervisor::attachment::receiver_gate::install(id);
        let (mut client, handler) = connection(&registry, id);
        tokio::pin!(handler);
        tokio::select! {
            _ = &mut handler => panic!("debugger ended before reaching the post-ack gate"),
            result = entered => result.unwrap(),
        }
        let ack = socket::read_json_line(&mut client).await.unwrap().unwrap();
        assert_eq!(ack["type"], "attached");
        assert!(ack["execution_id"].is_string(), "{ack}");

        // A expires after its live identity was acknowledged, before any lease.
        drop(a);
        release.send(()).unwrap();
        handler.await;
        while let Some(frame) = socket::read_json_line(&mut client).await.unwrap() {
            assert_ne!(frame["type"], "ready", "{frame}");
        }
        assert_eq!(
            registry
                .get_run_interactive(id)
                .await
                .unwrap()
                .2
                .borrow()
                .status,
            "running"
        );

        // No successor exists until the old handler has finished.
        let mut b = gremlin.next_interactive_session();
        assert!(!b.accept_pause(), "A's debugger reserved a pause on B");
        drop(b);
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn observer_end_without_command_successor_or_run_end_and_idle_attach() {
    timeout(Duration::from_secs(10), async {
        for mode in ["passive", "control"] {
            let id = format!("observer-end-idle-{mode}");
            let (registry, mut gremlin, _dir) = synthetic_run(&id).await;
            // Attach before even the first execution exists. Idle observers
            // must close, not wait for or reserve the first turn.
            let (mut initial, handler) = connection_mode(&registry, &id, Some(mode));
            tokio::spawn(handler).await.unwrap();
            while let Some(frame) = socket::read_json_line(&mut initial).await.unwrap() {
                assert_ne!(frame["type"], "ready", "{frame}");
            }
            let mut session = gremlin.next_interactive_session();
            assert!(!session.accept_pause(), "idle observer reserved first turn");
            let (mut client, handler) = connection_mode(&registry, &id, Some(mode));
            let task = tokio::spawn(handler);
            assert_eq!(
                socket::read_json_line(&mut client).await.unwrap().unwrap()["type"],
                "attached"
            );
            drop(session);
            task.await.unwrap();
            assert!(socket::read_json_line(&mut client).await.unwrap().is_none());
            assert_eq!(
                registry
                    .get_run_interactive(&id)
                    .await
                    .unwrap()
                    .2
                    .borrow()
                    .status,
                "running"
            );
            let (mut idle, handler) = connection_mode(&registry, &id, Some(mode));
            let task = tokio::spawn(handler);
            // Idle observation may close without acknowledgment, but must never
            // wait for a successor or reserve its pause.
            task.await.unwrap();
            while let Some(frame) = socket::read_json_line(&mut idle).await.unwrap() {
                assert_ne!(frame["type"], "ready", "{frame}");
            }
            let mut successor = gremlin.next_interactive_session();
            assert!(!successor.accept_pause());
            drop(successor);
            registry.stop_run(&id).await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn replacement_during_delivery_drops_correction_and_leaves_b_clean() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sandbox = Sandbox::with_providers(
        &json!({"openrouter": {
            "api-key":"test", "base-url":format!("http://{}", provider.local_addr().unwrap())
        }})
        .to_string(),
    );
    timeout(Duration::from_secs(15), async {
        let id = "replacement-during-delivery";
        let gremlin = create_chat_gremlin(
            id,
            &model("openrouter/replacement-b"),
            "helpful",
            sandbox.path(),
            &Default::default(),
        )
        .unwrap();
        let (registry, _) = spawn_test_actor().await;
        let mut gremlin = registry
            .register_chat_gremlin(id.into(), gremlin, CancelToken::new())
            .await;
        gremlin.state.set_stage("chat-0001", None, "");
        let mut session = gremlin.next_interactive_session();
        let (mut client, handler) = connection_mode(&registry, id, None);
        let task = tokio::spawn(handler);
        let ack = socket::read_json_line(&mut client).await.unwrap().unwrap();
        socket::write_json_line(
            client.get_mut(),
            &json!({
                "op":"message", "text":"A only correction", "req_id":"replace-in-flight"
            }),
        )
        .await
        .unwrap();
        wait_for(&mut client, |f| f["stage"] == "waiting_for_ready").await;
        assert!(session.accept_pause());
        let InteractiveCommand::Message(text, _, Some(delivery)) =
            session.next_command().await.unwrap()
        else {
            panic!("missing incorporation signal")
        };
        assert_eq!(text, "A only correction");
        // Replace A within the same live gremlin, keeping A and its
        // incorporation sender alive. Neither run-end nor sender EOF can
        // accidentally satisfy this execution-replacement test.
        let mut b_session = gremlin.next_interactive_session();
        assert!(!b_session.accept_pause(), "A's temporary pause leaked to B");
        assert_eq!(
            registry
                .get_run_interactive(id)
                .await
                .unwrap()
                .2
                .borrow()
                .status,
            "running"
        );
        let b_execution = registry
            .get_run_interactive(id)
            .await
            .unwrap()
            .0
            .target()
            .execution_id()
            .unwrap()
            .to_owned();
        assert_ne!(b_execution, ack["execution_id"].as_str().unwrap());
        emit_text(id, &b_execution, "B must not reach A");
        let agent = gremlin.client.run(
            RunParams {
                prompt: "B clean request".into(),
                gremlin_id: Some(id.into()),
                ..RunParams::empty()
            },
            b_session,
        );
        tokio::pin!(agent);
        let (request, _held) = tokio::select! {
            result = &mut agent => panic!("B stopped: {result:?}"),
            request = pending_request(&provider) => request,
        };
        assert!(request.to_string().contains("B clean request"));
        assert!(!request.to_string().contains("A only correction"));
        task.await.unwrap();
        let mut dropped = 0;
        while let Some(frame) = socket::read_json_line(&mut client).await.unwrap() {
            assert_ne!(frame["type"], "delivered", "{frame}");
            assert_ne!(frame["text"], "B must not reach A", "{frame}");
            if frame["type"] == "correction_dropped" {
                dropped += 1;
                assert_eq!(frame["req_id"], "replace-in-flight");
                assert_eq!(frame["execution_id"], ack["execution_id"]);
            }
        }
        assert_eq!(dropped, 1);
        drop(delivery);
        drop(session);
        registry.stop_run(id).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn slow_viewer_receiver_can_lag_without_blocking_owner() {
    timeout(Duration::from_secs(10), async {
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sandbox = Sandbox::with_providers(&json!({"openrouter": {
            "api-key":"test", "base-url":format!("http://{}", provider.local_addr().unwrap())
        }}).to_string());
        std::fs::write(sandbox.join("config/settings.yaml"), "default-model: openrouter/slow-viewer\n").unwrap();
        let (registry, _) = spawn_test_actor().await;
        let (session_id, session) = super::create_session(&registry).await;
        let (owner_client, owner_server) = UnixStream::pair().unwrap();
        let (server_read, server_write) = owner_server.into_split();
        let owner_task = tokio::spawn(crate::executor::supervisor::handle_session_main(
            server_read, SharedWriter::new(server_write), session_id.clone(), registry.clone(), session,
        ));
        let mut owner = BufReader::new(owner_client);
        socket::write_json_line(owner.get_mut(), &json!({
            "op":"chat", "session_id":session_id, "turn":"slow-owner", "text":"Finish all output",
            "project_root":sandbox.path(),
        })).await.unwrap();
        let ack = wait_for(&mut owner, |f| f["type"] == "ok").await;
        let id = ack["id"].as_str().unwrap();
        let (_, mut stream) = pending_request(&provider).await;
        let (entered, release) = crate::executor::supervisor::attachment::receiver_gate::install(id);
        let (mut client, handler) = connection_mode(&registry, id, Some("passive"));
        let task = tokio::spawn(handler);
        entered.await.unwrap();
        let ack = socket::read_json_line(&mut client).await.unwrap().unwrap();
        assert_eq!(ack["type"], "attached");
        stream.set_nodelay(true).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut expected = String::new();
        for index in 0..600 {
            let text = format!("chunk-{index} ");
            expected.push_str(&text);
            let chunk = format!("data: {}\n\n", json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}));
            stream.write_all(chunk.as_bytes()).await.unwrap();
            let frame = wait_for(&mut owner, |frame| frame["type"] == "stream_chunk").await;
            assert_eq!(frame["text"], text);
        }
        let done = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"done-slow","type":"function","function":{"name":"Done","arguments":json!({"summary":"all output intact"}).to_string()}}]},"finish_reason":"tool_calls"}]});
        stream.write_all(format!("data: {done}\n\ndata: [DONE]\n\n").as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let done_frame = wait_for(&mut owner, |frame| frame["type"] == "done").await;
        assert_eq!(done_frame["chat_turn"], "slow-owner");
        assert_eq!(done_frame["text"], expected);
        // Owner terminal output proves the TurnRunner and model finished while
        // the observer's receiver remains gated beyond its bounded capacity.
        release.send(()).unwrap();
        // Completion ends the snapshot while gated; it may close without
        // draining lagged frames. In either case owner output is already intact.
        drop(client);
        task.await.unwrap();
        drop(owner);
        owner_task.await.unwrap();
    }).await.unwrap();
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
    let gate = ChildGate::new();
    let cmd = gate.command();
    // A cmd-backed agent can finish without ever accepting a pause. Its
    // unacknowledged request must not reach the following native agent.
    let preflight_gate = ChildGate::new();
    let gated_model = format!("cmd/sh -c '{}'", preflight_gate.command());
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

    let mut exec = gate.entered().await;
    let assert_stage = || {
        let tree = store.state_tree();
        assert_eq!(tree["stage"], "pipeline");
        assert_eq!(tree["sub_stage"], "work");
    };
    assert_stage();

    // Control attach validates the active leaf before requesting a pause.
    let mut client_a = connect(&registry, &id);
    let rejected = next_response(&mut client_a).await;
    assert_eq!(rejected["type"], "error");
    assert!(rejected["message"]
        .as_str()
        .unwrap()
        .contains("not in an agent stage"));
    drop(client_a);
    exec.write_all(b"go\n").await.unwrap();
    drop(exec);

    // A command backend remains observable but cannot acquire control.
    let mut preflight = preflight_gate.entered().await;
    let mut expiring = connect(&registry, &id);
    let attached = socket::read_json_line(&mut expiring)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attached["type"], "attached", "{attached}");
    assert_eq!(attached["capabilities"], json!(["observe"]));
    preflight.write_all(b"go\n").await.unwrap();
    let expired = next_response(&mut expiring).await;
    assert_eq!(expired["type"], "error", "{expired}");

    // 3) Attach during native agent, send `message` — correction appears.
    let (initial, _held) = pending_request(&model).await;
    assert!(initial.to_string().contains("Review the changes."));
    assert_stage();
    let mut debug = connect(&registry, &id);
    let attached = socket::read_json_line(&mut debug).await.unwrap().unwrap();
    assert_eq!(attached["type"], "attached");
    assert_eq!(attached["scope"], "conversation");
    assert_eq!(attached["id"].as_str(), Some(id.as_str()));
    wait_for(&mut debug, |frame| {
        frame["type"] == "ready" && frame["id"].as_str() == Some(id.as_str())
    })
    .await;
    socket::write_json_line(
        debug.get_mut(),
        &json!({"op":"message", "text":"review the corrected implementation"}),
    )
    .await
    .unwrap();
    wait_for(&mut debug, |frame| frame["type"] == "delivered").await;
    socket::write_json_line(debug.get_mut(), &json!({"op":"quit"}))
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

    // Read the `attached` ack — the handler must be polled to write it.
    let ack = tokio::select! {
        _ = &mut handler => panic!("attach handler ended before attached ack"),
        frame = socket::read_json_line(&mut first) => frame.unwrap().unwrap(),
    };
    assert_eq!(ack["type"], "attached");

    tokio::select! {
        _ = &mut handler => panic!("attach handler ended before status frame"),
        // Wait for the status frame that follows the pause request.
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

    // Read `attached` from the second connection.
    let ack2 = socket::read_json_line(&mut second).await.unwrap().unwrap();
    assert_eq!(ack2["type"], "attached");

    tokio::select! {
        _ = &mut agent => panic!("agent exited instead of pausing again"),
        _ = wait_for(&mut second, |frame| frame["type"] == "ready" && frame["id"].as_str() == Some(id)) => {}
    }
    socket::write_json_line(
        second.get_mut(),
        &json!({"op": "message", "text": "try revision two"}),
    )
    .await
    .unwrap();
    tokio::select! {
        _ = &mut agent => panic!("agent exited before delivering second operator's message"),
        _ = wait_for(&mut second, |frame| frame["type"] == "delivered") => {}
    }
    socket::write_json_line(second.get_mut(), &json!({"op":"quit"}))
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

        // Control attach requests the pause before the first command.
        let (mut client, handler) = connection(&registry, id);
        let handler = tokio::spawn(handler);
        let ack = socket::read_json_line(&mut client).await.unwrap().unwrap();
        assert_eq!(ack["type"], "attached");

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

        // A fresh attachment must be able to request its own pause.
        let (mut replacement, handler) = connection(&registry, id);
        let handler = tokio::spawn(handler);
        let ack2 = socket::read_json_line(&mut replacement)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ack2["type"], "attached");
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
