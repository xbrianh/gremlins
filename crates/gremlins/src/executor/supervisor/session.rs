//! Session management: session lifecycle, socket accept loop, and teardown.
//!
//! All session state lives in [`super::actor`] behind [`super::actor::ActorMsg`].
//! This module is the IO layer: it owns the session sockets and translates
//! socket requests into actor messages.

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch};

use crate::executor::socket;

use super::actor::ActorMsg;
use super::actor_client::Client;
use super::chat::{handle_cancel_chat, handle_chat};
use super::debug::handle_debug;
use super::log_handler::handle_log;
use super::ops::{handle_chat_model, handle_compact, handle_session_stop};
use super::{error_response, ok_response, ConnectionGuard, SharedWriter};

/// Validate `id` as a session identifier.
pub(crate) fn validate_session_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.contains("..") {
        return Err(format!("invalid session id {id:?}"));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!("invalid session id {id:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session connections
// ---------------------------------------------------------------------------

/// Accept loop for one session socket.
/// First connection = session main; subsequent = aux (log, debug).
pub(crate) async fn run_session_listener(
    listener: UnixListener,
    session_id: String,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
) {
    const SESSION_MAIN_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    // Set once the main connection has shut down and the session has been
    // torn down. The accept loop watches it so it can stop accepting new aux
    // connections and drop the listener.
    let (death_tx, mut death_rx) = watch::channel(false);

    // The first connection must be the session main connection. Bound the
    // wait so a client that crashes between `create_session` and connect does
    // not leave a zombie session in the actor's session map forever.
    let first = tokio::time::timeout(SESSION_MAIN_CONNECT_TIMEOUT, listener.accept()).await;
    match first {
        Ok(Ok((stream, _addr))) => {
            let (read_half, write_half) = stream.into_split();
            let actor_tx_for_task = actor_tx.clone();
            let session_id_for_task = session_id.clone();
            let death_tx_for_task = death_tx.clone();
            let _main_handle = tokio::spawn(async move {
                let writer = SharedWriter::new(write_half);
                handle_session_main(read_half, writer, session_id_for_task, actor_tx_for_task)
                    .await;
                let _ = death_tx_for_task.send(true);
            });
        }
        Ok(Err(e)) => {
            log::error!("session {session_id}: first accept error: {e}");
            request_teardown(&actor_tx, &session_id, false).await;
            return;
        }
        Err(_) => {
            log::warn!("session {session_id}: timed out waiting for main connection");
            request_teardown(&actor_tx, &session_id, false).await;
            return;
        }
    }

    // Subsequent connections are auxiliary sockets on the same session
    // socket. The main connection death signal stops the loop without teardown
    // here: the main task owns teardown.
    loop {
        tokio::select! {
            _ = death_rx.changed() => {
                if *death_rx.borrow() {
                    break;
                }
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let (read_half, write_half) = stream.into_split();
                        let actor_tx_for_task = actor_tx.clone();
                        let session_id_for_task = session_id.clone();
                        let aux_task = tokio::spawn(async move {
                            handle_session_aux(
                                read_half,
                                write_half,
                                session_id_for_task,
                                actor_tx_for_task,
                            )
                            .await;
                        });
                        // Register the aux socket with the actor so teardown
                        // can abort it. A session that already closed refuses
                        // the connection (the task aborts itself).
                        Client::new(actor_tx.clone())
                            .session_push_aux_socket(&session_id, aux_task)
                            .await;
                    }
                    Err(e) => {
                        log::error!("session {session_id}: accept error: {e}");
                        request_teardown(&actor_tx, &session_id, false).await;
                        break;
                    }
                }
            }
        }
    }
}

/// Serve the session main connection. Reader and dispatcher run concurrently;
/// EOF on the reader marks the session closed.
pub(crate) async fn handle_session_main(
    main_read: OwnedReadHalf,
    writer: SharedWriter,
    session_id: String,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
) {
    // Hold the connection-count guard for the lifetime of the main connection.
    let _guard = ConnectionGuard::new(&actor_tx);

    let (request_tx, request_rx) = mpsc::channel::<Value>(64);

    let reader_actor_tx = actor_tx.clone();
    let reader_session_id = session_id.clone();
    let mut reader_handle = tokio::spawn(async move {
        run_session_main_reader(main_read, request_tx, reader_actor_tx, reader_session_id).await;
    });

    let dispatcher_actor_tx = actor_tx.clone();
    let dispatcher_writer = writer.clone();
    let dispatcher_session_id = session_id.clone();
    let mut dispatcher_handle = tokio::spawn(async move {
        run_session_dispatcher(
            request_rx,
            dispatcher_writer,
            dispatcher_session_id,
            dispatcher_actor_tx,
        )
        .await;
    });

    // Wait for either the socket to die or the dispatcher to finish. When
    // the reader exits first, let the dispatcher observe the session-closed
    // flag and drain queued requests before teardown. When the dispatcher
    // exits first (the graceful `close_session` path), stop the reader
    // immediately so the main writer and connection-count guard are released
    // without waiting for the client to close its write half.
    tokio::select! {
        result = &mut reader_handle => {
            let _ = result;
            let _ = dispatcher_handle.await;
        }
        result = &mut dispatcher_handle => {
            let _ = result;
            reader_handle.abort();
            let _ = reader_handle.await;
        }
    }
    request_teardown(&actor_tx, &session_id, true).await;
}

/// Read JSON-lines from main socket, forward to dispatcher. On EOF/error,
/// mark the session closed (the actor cancels the in-flight turn).
pub(crate) async fn run_session_main_reader(
    main_read: OwnedReadHalf,
    request_tx: mpsc::Sender<Value>,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
    session_id: String,
) {
    let mut reader = BufReader::new(main_read);

    loop {
        match socket::read_json_line(&mut reader).await {
            Ok(Some(request)) => {
                if request_tx.send(request).await.is_err() {
                    break;
                }
            }
            Ok(None) | Err(_) => {
                log::debug!("session {session_id}: main socket EOF/error");
                Client::new(actor_tx.clone())
                    .session_mark_closed(&session_id)
                    .await;
                break;
            }
        }
    }
}

pub(crate) enum SessionOp {
    Continue,
    CloseRequested,
}

/// Process session-socket requests sequentially (FIFO response order).
pub(crate) async fn run_session_dispatcher(
    mut request_rx: mpsc::Receiver<Value>,
    writer: SharedWriter,
    session_id: String,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
) {
    loop {
        let request = match request_rx.recv().await {
            Some(request) => request,
            None => break,
        };

        let op = request
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        log::debug!("session {session_id}: received op={op:?}");

        if let Err(resp) = verify_session_id(&request, &session_id) {
            let _ = writer.write_json_line(&resp).await;
            continue;
        }

        match handle_session_op(op.as_str(), &request, &writer, &session_id, &actor_tx).await {
            // `handle_session_op` already tore the session down (and awaited
            // it) before acknowledging, so the client cannot race ahead while
            // the gremlin is still live.
            SessionOp::CloseRequested => return,
            SessionOp::Continue => {}
        }

        // EOF may have been observed while this op was still running. Stop
        // processing queued requests for the now-dead session.
        if Client::new(actor_tx.clone())
            .session_is_closed(&session_id)
            .await
        {
            break;
        }
    }
}

pub(crate) async fn handle_session_op(
    op: &str,
    request: &Value,
    writer: &SharedWriter,
    session_id: &str,
    actor_tx: &mpsc::UnboundedSender<ActorMsg>,
) -> SessionOp {
    match op {
        "chat" => {
            handle_chat(request, writer, actor_tx, session_id).await;
            SessionOp::Continue
        }
        "chat_model" => {
            let resp = handle_chat_model(request, actor_tx, session_id).await;
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
        "cancel_chat" => {
            handle_cancel_chat(request, writer, actor_tx, session_id).await;
            SessionOp::Continue
        }
        "compact" => {
            let resp = handle_compact(request, actor_tx, session_id).await;
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
        "stop" => {
            let resp = handle_session_stop(request, actor_tx, session_id).await;
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
        "close_session" => {
            // Tear down before acknowledging so the client cannot race ahead
            // while the gremlin is still live.
            request_teardown(actor_tx, session_id, true).await;
            let _ = writer
                .write_json_line(&ok_response(serde_json::json!({})))
                .await;
            SessionOp::CloseRequested
        }
        _ => {
            let resp = error_response(&format!("unknown session op: {op:?}"));
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
    }
}

/// Serve a single auxiliary connection. First request names a streaming op
/// (`log`/`debug`/`stage_stream`).
pub(crate) async fn handle_session_aux(
    read_half: OwnedReadHalf,
    write_half: tokio::net::unix::OwnedWriteHalf,
    session_id: String,
    actor_tx: mpsc::UnboundedSender<ActorMsg>,
) {
    // Hold the connection-count guard for the lifetime of the aux connection.
    let _guard = ConnectionGuard::new(&actor_tx);
    let writer = SharedWriter::new(write_half);
    let mut reader = BufReader::new(read_half);

    let request = match socket::read_json_line(&mut reader).await {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(e) => {
            log::warn!("session {session_id}: aux read error: {e}");
            return;
        }
    };

    if let Err(resp) = verify_session_id(&request, &session_id) {
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let op = request.get("op").and_then(|v| v.as_str()).unwrap_or("");
    match op {
        "log" => {
            handle_log(&request, &actor_tx, reader, &writer).await;
        }
        "stage_stream" => {
            crate::executor::stage_stream::handle_stage_stream(
                &request, &actor_tx, reader, &writer,
            )
            .await;
        }
        "debug" => {
            handle_debug(&request, &actor_tx, reader, &writer).await;
        }
        _ => {
            let resp = error_response(&format!("unknown session op: {op:?}"));
            let _ = writer.write_json_line(&resp).await;
        }
    }
}

/// Verify `request` names the session this socket is bound to.
pub(crate) fn verify_session_id(request: &Value, expected: &str) -> Result<(), Value> {
    match request.get("session_id").and_then(|v| v.as_str()) {
        Some(id) if id == expected => Ok(()),
        Some(id) => {
            if validate_session_id(id).is_err() {
                return Err(error_response(&format!("invalid session id {id:?}")));
            }
            Err(error_response(&format!(
                "session id mismatch: request targets {id:?}, socket belongs to {expected:?}"
            )))
        }
        None => Err(error_response("missing 'session_id' field")),
    }
}

/// Ask the actor to tear down a session and wait for the drain to finish.
///
/// `abort_listener` stops the listener task before the socket is unlinked.
/// Callers *outside* the listener (the main connection) pass `true`; the
/// listener itself passes `false` so it does not abort its own task.
async fn request_teardown(
    actor_tx: &mpsc::UnboundedSender<ActorMsg>,
    session_id: &str,
    abort_listener: bool,
) {
    Client::new(actor_tx.clone())
        .session_teardown(session_id, abort_listener)
        .await;
}
