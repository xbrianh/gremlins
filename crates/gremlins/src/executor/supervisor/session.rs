//! Session management: session lifecycle, socket accept loop, and teardown.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::executor::gremlin::Gremlin;
use crate::executor::socket;

use super::chat::{cleanup_chat_gremlin, handle_cancel_chat, handle_chat};
use super::debug::handle_debug;
use super::event::ConnectionGuard;
use super::log_handler::handle_log;
use super::ops::{handle_chat_model, handle_compact, handle_session_stop};
use super::{error_response, ok_response, CurrentTurn, SharedWriter};

/// Daemon-side assets for one interactive front-end session.
///
/// Owns chat gremlins, the in-flight turn, and all session-socket connections.
/// Lifetime is bound to the main session-socket connection; teardown drops
/// gremlins and closes aux sockets.
pub(crate) struct ClientSession {
    pub(crate) session_id: String,
    /// The task that owns the session's [`UnixListener`]. Stored so teardown
    /// can stop accepting new connections before the socket is unlinked.
    pub(crate) listener_task: Option<tokio::task::JoinHandle<()>>,
    /// Chat gremlins currently parked in the session, keyed by gremlin id.
    /// A gremlin is moved out while a turn is running and re-inserted when
    /// the turn finishes.
    pub(crate) chat_gremlins: HashMap<String, Gremlin>,
    /// Gremlin ids owned by this session. Kept separate from
    /// `chat_gremlins` because an in-flight turn temporarily removes its
    /// gremlin from the map without relinquishing ownership.
    pub(crate) chat_ids: HashSet<String>,
    /// The in-flight chat turn, if any.
    pub(crate) current_turn: Option<CurrentTurn>,
    /// Auxiliary socket tasks (log follow, debug) opened on the session
    /// socket. Aborted when the session dies.
    pub(crate) aux_sockets: Vec<tokio::task::JoinHandle<()>>,
    /// Set when the main socket reaches EOF/error; stops the dispatcher and
    /// rejects newly-arriving aux connections. Distinct from `torn_down`:
    /// teardown still runs after request-side closure so session resources
    /// are drained exactly once on the EOF path.
    pub(crate) closed: bool,
    /// Set when teardown has begun. Teardown is idempotent under this flag,
    /// but the flag is *not* set by request-side closure (`closed`).
    pub(crate) torn_down: bool,
    /// Session-scoped set of active skill names. Shared with the agent loop
    /// so the LoadSkill tool can add names mid-turn. `/clear` resets it.
    pub(crate) active_skills: Arc<Mutex<Vec<String>>>,
    /// Skill names the operator explicitly requested on the last turn
    /// (the `skills` field from the chat request). Used to compute the
    /// delta when merging operator toggles with model LoadSkill pushes.
    pub(crate) operator_skills: Vec<String>,
}

impl ClientSession {
    pub(crate) fn new(session_id: String) -> Self {
        Self {
            session_id,
            listener_task: None,
            chat_gremlins: HashMap::new(),
            chat_ids: HashSet::new(),
            current_turn: None,
            aux_sockets: Vec::new(),
            closed: false,
            torn_down: false,
            active_skills: Arc::new(Mutex::new(Vec::new())),
            operator_skills: Vec::new(),
        }
    }

    /// Cancel the current turn only if it belongs to `id`. Returns the done
    /// receiver so the caller can wait for the turn to drain.
    pub(crate) fn cancel_turn_for_id(&self, id: &str) -> Option<watch::Receiver<bool>> {
        self.current_turn.as_ref().and_then(|cur| {
            if cur.id == id {
                cur.soft_cancel.store(false, Ordering::Relaxed);
                cur.cancel.cancel();
                Some(cur.done.clone())
            } else {
                None
            }
        })
    }

    /// Hard-cancel the current turn regardless of which gremlin owns it.
    /// Returns the done receiver so the caller can wait for the turn to drain.
    pub(crate) fn cancel_current_turn(&self) -> Option<watch::Receiver<bool>> {
        self.current_turn.as_ref().map(|cur| {
            cur.soft_cancel.store(false, Ordering::Relaxed);
            cur.cancel.cancel();
            cur.done.clone()
        })
    }
}

/// Shared handle to a [`ClientSession`], keyed by session id.
pub(crate) type SessionHandle = Arc<tokio::sync::Mutex<ClientSession>>;

/// Global session map. Ids are unforgeable, returned by `create_session`.
static SESSION_MAP: OnceLock<Arc<Mutex<HashMap<String, SessionHandle>>>> = OnceLock::new();

pub(crate) fn session_map() -> &'static Arc<Mutex<HashMap<String, SessionHandle>>> {
    SESSION_MAP.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

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
    session: SessionHandle,
    state_root: PathBuf,
    shutdown_tx: watch::Sender<bool>,
) {
    const SESSION_MAIN_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    // Set once the main connection has shut down and the session has been
    // torn down. The accept loop watches it so it can stop accepting new
    // aux connections and drop the listener.
    let (death_tx, mut death_rx) = watch::channel(false);
    let mut shutdown_rx = shutdown_tx.subscribe();

    // The first connection must be the session main connection. Bound the
    // wait so a client that crashes between `create_session` and connect
    // does not leave a zombie session in `SESSION_MAP` forever.
    let first = tokio::time::timeout(SESSION_MAIN_CONNECT_TIMEOUT, listener.accept()).await;
    match first {
        Ok(Ok((stream, _addr))) => {
            let (read_half, write_half) = stream.into_split();
            let session_for_task = Arc::clone(&session);
            let state_root_for_task = state_root.clone();
            let shutdown_tx_for_task = shutdown_tx.clone();
            let death_tx_for_task = death_tx.clone();
            let _main_handle = tokio::spawn(async move {
                let _guard = ConnectionGuard::new(&shutdown_tx_for_task);
                let writer = SharedWriter::new(write_half);
                handle_session_main(
                    read_half,
                    writer,
                    Arc::clone(&session_for_task),
                    &state_root_for_task,
                )
                .await;
                let _ = death_tx_for_task.send(true);
            });
        }
        Ok(Err(e)) => {
            log::error!("session {session_id}: first accept error: {e}");
            teardown_session_from_listener(&session, &state_root).await;
            return;
        }
        Err(_) => {
            log::warn!("session {session_id}: timed out waiting for main connection");
            teardown_session_from_listener(&session, &state_root).await;
            return;
        }
    }

    // Subsequent connections are auxiliary sockets on the same session
    // socket. The main connection death signal and the supervisor shutdown
    // signal both stop the loop without torn-down here: the main task and
    // `run_supervisor` own teardown respectively.
    let mut tear_down_on_exit = false;
    loop {
        tokio::select! {
            _ = death_rx.changed() => {
                if *death_rx.borrow() {
                    break;
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let (read_half, write_half) = stream.into_split();
                        let session_for_task = Arc::clone(&session);
                        let state_root_for_task = state_root.clone();
                        let shutdown_tx_for_task = shutdown_tx.clone();
                        let aux_task = tokio::spawn(async move {
                            let _guard = ConnectionGuard::new(&shutdown_tx_for_task);
                            handle_session_aux(
                                read_half,
                                write_half,
                                Arc::clone(&session_for_task),
                                &state_root_for_task,
                            )
                            .await;
                        });
                        let mut s = session.lock().await;
                        if s.closed {
                            aux_task.abort();
                        } else {
                            s.aux_sockets.push(aux_task);
                        }
                    }
                    Err(e) => {
                        log::error!("session {session_id}: accept error: {e}");
                        tear_down_on_exit = true;
                        break;
                    }
                }
            }
        }
    }

    if tear_down_on_exit {
        teardown_session_from_listener(&session, &state_root).await;
    }
}

/// Serve the session main connection. Reader and dispatcher run concurrently;
/// EOF on the reader marks the session closed.
pub(crate) async fn handle_session_main(
    main_read: OwnedReadHalf,
    writer: SharedWriter,
    session: SessionHandle,
    state_root: &Path,
) {
    let (request_tx, request_rx) = mpsc::channel::<Value>(64);

    let reader_session = Arc::clone(&session);
    let mut reader_handle = tokio::spawn(async move {
        run_session_main_reader(main_read, request_tx, reader_session).await;
    });

    let dispatcher_session = Arc::clone(&session);
    let dispatcher_writer = writer.clone();
    let dispatcher_state_root = state_root.to_path_buf();
    let mut dispatcher_handle = tokio::spawn(async move {
        run_session_dispatcher(
            request_rx,
            dispatcher_writer,
            dispatcher_session,
            dispatcher_state_root,
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
    teardown_session(&session, state_root).await;
}

/// Read JSON-lines from main socket, forward to dispatcher. On EOF/error,
/// mark session closed and cancel in-flight turn.
pub(crate) async fn run_session_main_reader(
    main_read: OwnedReadHalf,
    request_tx: mpsc::Sender<Value>,
    session: SessionHandle,
) {
    let mut reader = BufReader::new(main_read);
    let session_id = {
        let s = session.lock().await;
        s.session_id.clone()
    };

    loop {
        match socket::read_json_line(&mut reader).await {
            Ok(Some(request)) => {
                if request_tx.send(request).await.is_err() {
                    break;
                }
            }
            Ok(None) | Err(_) => {
                log::debug!("session {session_id}: main socket EOF/error");
                let mut s = session.lock().await;
                if !s.closed {
                    s.closed = true;
                    if let Some(cur) = &s.current_turn {
                        cur.cancel.cancel();
                    }
                }
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
    session: SessionHandle,
    state_root: PathBuf,
) {
    let session_id = {
        let s = session.lock().await;
        s.session_id.clone()
    };

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

        match handle_session_op(op.as_str(), &request, &state_root, &writer, &session).await {
            SessionOp::CloseRequested => {
                // Tear down before acknowledging so the client cannot race
                // ahead while the gremlin is still live.
                teardown_session(&session, &state_root).await;
                let _ = writer
                    .write_json_line(&ok_response(serde_json::json!({})))
                    .await;
                return;
            }
            SessionOp::Continue => {}
        }

        // EOF may have been observed while this op was still running. Stop
        // processing queued requests for the now-dead session.
        if session_is_closed(&session).await {
            break;
        }
    }
}

pub(crate) async fn handle_session_op(
    op: &str,
    request: &Value,
    state_root: &Path,
    writer: &SharedWriter,
    session: &SessionHandle,
) -> SessionOp {
    match op {
        "chat" => {
            handle_chat(request, state_root, writer, session).await;
            SessionOp::Continue
        }
        "chat_model" => {
            let resp = handle_chat_model(request, session).await;
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
        "cancel_chat" => {
            handle_cancel_chat(request, writer, session).await;
            SessionOp::Continue
        }
        "compact" => {
            let resp = handle_compact(request, session).await;
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
        "stop" => {
            let resp = handle_session_stop(request, session).await;
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
        "close_session" => SessionOp::CloseRequested,
        _ => {
            let resp = error_response(&format!("unknown session op: {op:?}"));
            let _ = writer.write_json_line(&resp).await;
            SessionOp::Continue
        }
    }
}

pub(crate) async fn session_is_closed(session: &SessionHandle) -> bool {
    session.lock().await.closed
}

/// Serve a single auxiliary connection. First request names a streaming op
/// (`log`/`debug`).
pub(crate) async fn handle_session_aux(
    read_half: OwnedReadHalf,
    write_half: tokio::net::unix::OwnedWriteHalf,
    session: SessionHandle,
    state_root: &Path,
) {
    let writer = SharedWriter::new(write_half);
    let mut reader = BufReader::new(read_half);
    let session_id = {
        let s = session.lock().await;
        s.session_id.clone()
    };

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
            handle_log(&request, state_root, reader, &writer).await;
        }
        "stage_stream" => {
            crate::executor::stage_stream::handle_stage_stream(
                &request, state_root, reader, &writer,
            )
            .await;
        }
        "debug" => {
            handle_debug(&request, state_root, reader, &writer).await;
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

/// Tear down a session: cancel in-flight turn, abort aux sockets, drop chat
/// gremlins, remove session state, unlink session socket.
pub(crate) async fn teardown_session(session: &SessionHandle, state_root: &Path) {
    teardown_session_impl(session, state_root, true).await;
}

/// Listener-exit variant of [`teardown_session`] — does NOT abort the listener.
pub(crate) async fn teardown_session_from_listener(session: &SessionHandle, state_root: &Path) {
    teardown_session_impl(session, state_root, false).await;
}

pub(crate) async fn teardown_session_impl(
    session: &SessionHandle,
    state_root: &Path,
    abort_listener: bool,
) {
    let (chat_ids, chat_gremlins, aux_sockets, listener_task, session_id, turn_done) = {
        let mut s = session.lock().await;
        if s.torn_down {
            return;
        }
        s.torn_down = true;
        s.closed = true;

        let turn_done = s.cancel_current_turn();

        let chat_ids: Vec<String> = s.chat_ids.drain().collect();
        let chat_gremlins = std::mem::take(&mut s.chat_gremlins);
        let aux_sockets = std::mem::take(&mut s.aux_sockets);
        let listener_task = s.listener_task.take();
        (
            chat_ids,
            chat_gremlins,
            aux_sockets,
            listener_task,
            s.session_id.clone(),
            turn_done,
        )
    };

    if abort_listener {
        if let Some(listener_task) = listener_task {
            listener_task.abort();
        }
    } else {
        drop(listener_task);
    }

    // Abort aux sockets and await them so their connection-count guards run
    // before we reap the gremlins and remove the session.
    for aux in aux_sockets {
        aux.abort();
        let _ = aux.await;
    }

    // Wait for a cancelled in-flight turn to finish and relinquish its
    // `Gremlin` before we clean up the run map entry it may still touch.
    if let Some(mut done) = turn_done {
        let _ = done.wait_for(|v| *v).await;
    }

    for id in &chat_ids {
        cleanup_chat_gremlin(id).await;
    }

    // Drop any parked gremlins only after `cleanup_chat_gremlin` has removed
    // their run-map entries and reaped their clients.
    drop(chat_gremlins);

    session_map().lock().unwrap().remove(&session_id);
    socket::unlink_session_socket(state_root, &session_id);
}
