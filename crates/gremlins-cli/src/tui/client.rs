//! Persistent socket client for the TUI.
//!
//! The client holds two long-lived connections to the executor daemon:
//!
//! * the shared **control socket** (`executor.sock`) for `create_session`
//!   and the pipeline ops (`ls`, `launch`, `stop`, `resume`, `info`,
//!   `status`, `artifact`)
//! * one **session socket** (`sess-<id>.sock`) for chat and other
//!   session-scoped ops
//!
//! The daemon routes daemon broadcasts over the control socket and chat
//! stream events over the session socket, so each connection owns its own
//! response FIFO and its own background read task.

pub(crate) mod attachment;

use std::collections::VecDeque;
use std::sync::Arc;

use super::protocol::{Execution, Selector};
use gremlins::executor::socket;
use gremlins_magic_bus::EventKind;
use serde_json::{json, Value};

use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex};

/// A home event, its client-side turn tag, and (only at turn start) its execution.
pub(super) type ChatEvent = (EventKind, Option<String>, Option<Execution>);

/// Outbound side of a single daemon socket connection.
struct OutState {
    /// Daemon lifecycle events forwarded to the TUI event loop.
    event_tx: mpsc::UnboundedSender<EventKind>,
    /// Chat stream events tagged with the client turn id for staleness filtering.
    chat_tx: mpsc::UnboundedSender<ChatEvent>,
    /// Raw JSON lines that are neither responses nor EventKinds.
    raw_tx: mpsc::UnboundedSender<Value>,
}

/// One persistent socket connection and its background reader.
struct Connection {
    /// Outbound socket plus, for session connections, the session id that
    /// belongs to that socket. Guarded by one lock so a send can never write
    /// a request stamped with the previous session id to the new socket.
    write: Mutex<WriteState>,
    /// Pending response waiters in request order. The daemon processes
    /// requests sequentially on a connection and writes each response in
    /// order, so FIFO delivery is correct.
    responses: Arc<Mutex<VecDeque<oneshot::Sender<Value>>>>,
    /// Handle for the background read-loop task. Aborted on shutdown to
    /// close the socket read half.
    read_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Classified-output senders shared by the connection's read loop.
    out: Arc<OutState>,
}

struct WriteState {
    write_half: OwnedWriteHalf,
    /// `Some` for the session connection; `None` for the control socket.
    session_id: Option<String>,
    /// Registry-reserved home identity belonging to the same session socket.
    home_run_id: Option<String>,
    /// Monotonic generation of the installed session. Incremented each time
    /// [`Connection::restart`] swaps in a replacement session socket.
    generation: u64,
}

impl Connection {
    fn new_control(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        out: Arc<OutState>,
    ) -> Self {
        Self::new(reader, write_half, None, None, out)
    }

    fn new_session(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        session_id: String,
        home_run_id: String,
        out: Arc<OutState>,
    ) -> Self {
        Self::new(reader, write_half, Some(session_id), Some(home_run_id), out)
    }

    fn new(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        session_id: Option<String>,
        home_run_id: Option<String>,
        out: Arc<OutState>,
    ) -> Self {
        let responses: Arc<Mutex<VecDeque<oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let read_out = Arc::clone(&out);
        let read_responses = Arc::clone(&responses);
        let read_handle = tokio::spawn(async move {
            read_loop(reader, read_out, read_responses).await;
        });

        Self {
            write: Mutex::new(WriteState {
                write_half,
                session_id,
                home_run_id,
                generation: 0,
            }),
            responses,
            read_handle: Mutex::new(Some(read_handle)),
            out,
        }
    }

    /// Send a JSON request and wait for the response.
    ///
    /// Serialises waiter registration and the socket write under the same
    /// lock so that request order on the wire matches response order.
    async fn send(&self, request: Value) -> Result<Value, String> {
        self.send_with(request, false, None).await
    }

    /// Send a request on the session connection, stamping the session id
    /// that is atomically paired with the current session write half.
    async fn send_on_session(&self, request: Value) -> Result<Value, String> {
        self.send_with(request, true, None).await
    }

    /// Send a session request only if the connection still belongs to the
    /// session generation observed at submission time.
    async fn send_on_session_for_generation(
        &self,
        request: Value,
        expected_generation: u64,
    ) -> Result<Value, String> {
        self.send_with(request, true, Some(expected_generation))
            .await
    }

    async fn send_with(
        &self,
        mut request: Value,
        session_scoped: bool,
        expected_generation: Option<u64>,
    ) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();

        // Hold the write lock across stamping, enqueue, and write so
        // concurrent callers cannot interleave and a session restart cannot
        // publish a new write half while a request is being stamped.
        let mut write = self.write.lock().await;
        if let Some(expected) = expected_generation {
            if write.generation != expected {
                return Err(format!(
                    "session was recreated (expected generation {expected}, current {})",
                    write.generation
                ));
            }
        }
        if session_scoped {
            let session_id = write
                .session_id
                .clone()
                .ok_or_else(|| "session connection not initialized".to_string())?;
            if let Some(obj) = request.as_object_mut() {
                obj.insert("session_id".to_string(), Value::String(session_id));
            } else {
                return Err("request body must be a JSON object".to_string());
            }
        }

        {
            let mut guard = self.responses.lock().await;
            guard.push_back(tx);
        }

        if let Err(e) = socket::write_json_line(&mut write.write_half, &request).await {
            // Write failed — remove our waiter so the queue stays aligned.
            let mut guard = self.responses.lock().await;
            guard.pop_back();
            return Err(e);
        }
        drop(write);

        rx.await.map_err(|_| "connection closed".to_string())
    }

    /// Abort, await, and replace the current read loop plus its write half
    /// (used after `/clear` recreates the session socket).
    async fn restart(
        &self,
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        new_session_id: String,
        new_home_run_id: String,
    ) {
        // Abort and await the old reader first. Awaiting guarantees its
        // terminal queue clear can no longer race the new connection's
        // waiters.
        if let Some(handle) = self.read_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        // Hold the write lock across queue draining, write-half replacement,
        // session-id publication, and read-loop installation so a concurrent
        // send can neither enqueue against the old connection nor have its
        // waiter dropped after the fact.
        let mut write = self.write.lock().await;
        write.write_half = write_half;
        write.session_id = Some(new_session_id);
        write.home_run_id = Some(new_home_run_id);
        write.generation += 1;
        self.responses.lock().await.clear();

        let read_out = Arc::clone(&self.out);
        let read_responses = Arc::clone(&self.responses);
        let read_handle = tokio::spawn(async move {
            read_loop(reader, read_out, read_responses).await;
        });
        *self.read_handle.lock().await = Some(read_handle);
        drop(write);
    }

    /// The session id paired with the current session write half.
    async fn current_session_id(&self) -> String {
        self.write
            .lock()
            .await
            .session_id
            .clone()
            .unwrap_or_default()
    }

    /// The generation of the session installed in the current write half.
    async fn current_generation(&self) -> u64 {
        self.write.lock().await.generation
    }

    /// Abort the background read loop, closing the socket read half.
    async fn shutdown(&self) {
        if let Some(handle) = self.read_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

/// A persistent client for the executor daemon.
///
/// `control` is the shared executor socket; `session` is the current
/// interactive session socket. The two are independent: chat turns stream
/// over the session socket while daemon broadcasts and pipeline ops still
/// use the control socket.
pub struct DaemonClient {
    control: Connection,
    session: Connection,
    out: Arc<OutState>,
    /// Serializes session recreation so a `/clear` can never race another
    /// session operation's restart and drop un-acked waiters.
    session_recreate: Mutex<()>,
    #[cfg(test)]
    aux_streams: Mutex<Option<VecDeque<tokio::net::UnixStream>>>,
}

impl DaemonClient {
    /// Build a fake from the same authoritative setup response as a real connection.
    #[cfg(test)]
    pub(super) fn fake_with_identity(response: Value) -> (Self, [tokio::net::UnixStream; 2]) {
        Self::fake_with_aux_streams(response, Vec::new())
    }

    #[cfg(test)]
    pub(super) fn fake_with_aux_streams(
        response: Value,
        streams: Vec<tokio::net::UnixStream>,
    ) -> (Self, [tokio::net::UnixStream; 2]) {
        let (session_id, home_run_id) = parse_session_setup(&response).unwrap();
        let (control, control_peer) = tokio::net::UnixStream::pair().unwrap();
        let (session, session_peer) = tokio::net::UnixStream::pair().unwrap();
        let out = Arc::new(OutState {
            event_tx: mpsc::unbounded_channel().0,
            chat_tx: mpsc::unbounded_channel().0,
            raw_tx: mpsc::unbounded_channel().0,
        });
        let (control_read, control_write) = control.into_split();
        let (session_read, session_write) = session.into_split();
        (
            Self {
                control: Connection::new_control(
                    BufReader::new(control_read),
                    control_write,
                    Arc::clone(&out),
                ),
                session: Connection::new_session(
                    BufReader::new(session_read),
                    session_write,
                    session_id,
                    home_run_id,
                    Arc::clone(&out),
                ),
                out,
                session_recreate: Mutex::new(()),
                aux_streams: Mutex::new(Some(streams.into())),
            },
            [control_peer, session_peer],
        )
    }

    /// The current session id, atomically paired with the session socket in
    /// [`Connection::write`].
    pub async fn session_id(&self) -> String {
        self.session.current_session_id().await
    }

    /// The registry-reserved home run id paired with the current session socket.
    pub async fn home_run_id(&self) -> String {
        self.session
            .write
            .lock()
            .await
            .home_run_id
            .clone()
            .expect("session connection has a home run id")
    }

    /// The generation of the current session socket. Callers capture this
    /// at submission time and pass it to
    /// [`send_session_request_for_generation`](Self::send_session_request_for_generation)
    /// so a request that lost a `/clear` race can never execute in the
    /// replacement session.
    pub async fn session_generation(&self) -> u64 {
        self.session.current_generation().await
    }

    /// Send a request on the control socket.
    pub async fn send_control_request(&self, request: Value) -> Result<Value, String> {
        self.control.send(request).await
    }

    /// Send a request on the session socket. The current session id is
    /// stamped into the request under the same lock that publishes the
    /// session write half, so a `/clear` restart can never write a request
    /// bearing the old id to the new socket.
    pub async fn send_session_request(&self, request: Value) -> Result<Value, String> {
        self.session.send_on_session(request).await
    }

    /// Send a session request only if the connection still belongs to
    /// `expected_generation`.
    ///
    /// The generation check and session-id stamp happen atomically under the
    /// connection's write lock, so a request submitted before a `/clear`
    /// cannot be stamped with the replacement session's id even when its
    /// send is delayed (for example by repository-context collection).
    pub async fn send_session_request_for_generation(
        &self,
        request: Value,
        expected_generation: u64,
    ) -> Result<Value, String> {
        // Queue behind any in-progress `/clear` recreation. Once the new
        // session is installed, the generation check below rejects a request
        // that was submitted against a retired session instead of writing it
        // to the old (already-closed) socket or stamping it with the new id.
        let _recreate_guard = self.session_recreate.lock().await;
        self.session
            .send_on_session_for_generation(request, expected_generation)
            .await
    }

    /// Open an auxiliary connection on the session socket.
    ///
    /// `log`/`debug` streaming ops monopolise their connection, so the TUI
    /// opens a dedicated session connection for them. The daemon tracks
    /// these connections as session-owned and closes them when the session
    /// dies.
    pub async fn open_aux_stream(&self) -> Result<tokio::net::UnixStream, String> {
        #[cfg(test)]
        if let Some(streams) = self.aux_streams.lock().await.as_mut() {
            return streams
                .pop_front()
                .ok_or_else(|| "no fake auxiliary stream available".to_owned());
        }
        let state_root = gremlins::config::state_root();
        socket::connect_session_socket(&state_root, &self.session_id().await).await
    }

    async fn attach_conversation(
        &self,
        selector: Selector<'_>,
    ) -> Result<(BufReader<tokio::net::UnixStream>, Execution), String> {
        let session = self.session_id().await;
        let stream =
            socket::connect_session_socket(&gremlins::config::state_root(), &session).await?;
        attach_conversation(stream, &session, selector).await
    }

    /// Send exactly one correction on a socket pinned to the operator's execution.
    /// This connection never forwards display frames to either TUI event stream.
    pub async fn deliver_correction(
        &self,
        execution: &Execution,
        id: u64,
        text: &str,
    ) -> Result<(), String> {
        let (stream, bound) = self.attach_conversation(Selector::Exact(execution)).await?;
        send_correction(stream, &bound, id, text).await
    }

    /// Open a dedicated session connection for a streaming log follow.
    pub async fn follow_log(
        &self,
        id: &str,
    ) -> Result<(mpsc::UnboundedReceiver<Value>, tokio::task::JoinHandle<()>), String> {
        let session_id = self.session_id().await;
        let mut stream = self.open_aux_stream().await?;

        let request =
            serde_json::json!({"op": "log", "id": id, "follow": true, "session_id": session_id});
        socket::write_json_line(&mut stream, &request).await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            while let Some(line) = socket::read_json_line(&mut reader).await.ok().flatten() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        Ok((rx, handle))
    }

    /// Passively observe a run on a dedicated session connection.
    /// Requires replay-checkpoint support in the acknowledgment before yielding native frames.
    pub async fn follow_run(
        &self,
        id: &str,
    ) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String> {
        let session_id = self.session_id().await;
        let stream = self.open_aux_stream().await?;
        follow_run(stream, &session_id, id).await
    }

    /// Read persisted history together with its native handoff watermark.
    pub async fn read_blob_checkpoint(
        &self,
        id: &str,
        blob: &str,
        tail: usize,
    ) -> Result<(Vec<String>, Option<u64>, Option<Vec<u64>>), String> {
        let request = serde_json::json!({
            "op": "read_blob",
            "id": id,
            "blob": blob,
            "tail": tail,
        });
        let resp = self.send_session_request(request).await?;
        if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
            return Err(resp
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }
        let lines: Vec<String> = resp
            .get("lines")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let covered_sequences = resp["covered_sequences"]
            .as_array()
            .map(|sequences| sequences.iter().filter_map(Value::as_u64).collect());
        Ok((lines, resp["through_sequence"].as_u64(), covered_sequences))
    }

    /// Read the gremlin's hermetic definition tree.
    pub async fn read_definition(&self, id: &str) -> Result<serde_json::Value, String> {
        let request = serde_json::json!({
            "op": "read_definition",
            "id": id,
        });
        let resp = self.send_session_request(request).await?;
        if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
            return Err(resp
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }
        Ok(resp)
    }

    /// Close the current session and create a fresh one.
    ///
    /// Used by `/clear` (releases the old chat gremlin and starts a fresh
    /// conversation) rather than the old `stop` + client-side `chat_id`
    /// dance. The old session socket is closed by the daemon after its
    /// `close_session` ack.
    pub async fn recreate_session(&self) -> Result<(), String> {
        let _recreate_guard = self.session_recreate.lock().await;

        let close_request = serde_json::json!({"op": "close_session"});
        let _ = self.session.send_on_session(close_request).await?;

        let create_request = serde_json::json!({"op": "create_session"});
        let resp = self.control.send(create_request).await?;
        let (new_session_id, new_home_run_id) = parse_session_setup(&resp)?;

        let state_root = gremlins::config::state_root();
        let stream = socket::connect_session_socket(&state_root, &new_session_id).await?;
        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);
        self.session
            .restart(reader, write_half, new_session_id, new_home_run_id)
            .await;
        Ok(())
    }

    /// Abort both background read loops, closing the sockets.
    pub async fn shutdown(&self) {
        self.control.shutdown().await;
        self.session.shutdown().await;
    }

    /// A sender for daemon events, so a caller can inject a locally-detected
    /// error (e.g. a failed chat request) into the same stream the TUI's
    /// event loop consumes.
    pub fn chat_sender(&self) -> mpsc::UnboundedSender<ChatEvent> {
        self.out.chat_tx.clone()
    }
}

async fn attach_conversation(
    stream: tokio::net::UnixStream,
    session: &str,
    selector: Selector<'_>,
) -> Result<(BufReader<tokio::net::UnixStream>, Execution), String> {
    let mut stream = BufReader::new(stream);
    socket::write_json_line(stream.get_mut(), &selector.request(session)).await?;
    let ack = attachment::read_ack(&mut stream).await?;
    Ok((stream, selector.binding(&ack)?))
}

async fn send_correction(
    mut stream: BufReader<tokio::net::UnixStream>,
    execution: &Execution,
    id: u64,
    text: &str,
) -> Result<(), String> {
    let req_id = id.to_string();
    let message = json!({"op":"message", "req_id":req_id, "text":text});
    let uncertain = |reason: String| format!("delivery uncertain: {reason}");
    socket::write_json_line(stream.get_mut(), &message)
        .await
        .map_err(uncertain)?;
    while let Some(frame) = socket::read_json_line(&mut stream)
        .await
        .map_err(uncertain)?
    {
        match frame["type"].as_str() {
            Some(kind @ ("delivered" | "correction_dropped")) => {
                if frame["scope"] != "conversation"
                    || frame["req_id"] != req_id
                    || Execution::from_frame(&frame).as_ref() != Some(execution)
                {
                    return Err(uncertain("mismatched acknowledgment".to_owned()));
                }
                return if kind == "delivered" {
                    Ok(())
                } else {
                    Err("correction not delivered".to_owned())
                };
            }
            // Errors from the command handler are unscoped. A scoped error
            // is a display event from the conversation, not a rejection.
            Some("error") if frame.get("scope").is_none() => {
                return Err(frame["message"]
                    .as_str()
                    .unwrap_or("correction rejected")
                    .to_owned())
            }
            _ => {}
        }
    }
    Err(uncertain(
        "attachment closed without confirmation".to_owned(),
    ))
}

async fn follow_run(
    mut stream: tokio::net::UnixStream,
    session_id: &str,
    id: &str,
) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String> {
    let request =
        json!({"op":"attach", "scope":"run", "id":id, "session_id":session_id, "replay":true});
    socket::write_json_line(&mut stream, &request).await?;
    let mut reader = BufReader::new(stream);
    let ack = socket::read_json_line(&mut reader)
        .await?
        .ok_or("executor closed connection before run attachment acknowledgment")?;
    if ack["type"] == "error" {
        return Err(ack["message"]
            .as_str()
            .unwrap_or("run attachment failed")
            .to_owned());
    }
    if ack["type"] != "attached" || ack["scope"] != "run" || ack["id"] != id {
        return Err(format!("unexpected run attachment acknowledgment: {ack}"));
    }

    if !ack["capabilities"].as_array().is_some_and(|capabilities| {
        capabilities
            .iter()
            .any(|capability| capability == "replay_checkpoint")
    }) {
        return Err("run attachment does not support replay_checkpoint".to_owned());
    }

    let (tx, rx) = mpsc::channel(256);
    let handle = tokio::spawn(async move {
        while let Some(frame) = socket::read_json_line(&mut reader).await.ok().flatten() {
            if tx.send(frame).await.is_err() {
                break;
            }
        }
    });
    Ok((rx, handle))
}

fn parse_session_setup(response: &Value) -> Result<(String, String), String> {
    if response.get("type").and_then(Value::as_str) == Some("error") {
        return Err(response
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned());
    }
    let required = |field: &str| {
        response
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("create_session response missing '{field}'"))
    };
    Ok((required("session_id")?, required("home_run_id")?))
}

/// Connect to the executor daemon, create a session, and return the client
/// handle plus receivers for unsolicited [`EventKind`] values (lifecycle events),
/// chat events tagged with the client turn id, and raw JSON lines.
pub async fn connect() -> Result<
    (
        DaemonClient,
        mpsc::UnboundedReceiver<EventKind>,
        mpsc::UnboundedReceiver<ChatEvent>,
        mpsc::UnboundedReceiver<Value>,
    ),
    String,
> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (chat_tx, chat_rx) = mpsc::unbounded_channel();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel();
    let out = Arc::new(OutState {
        event_tx,
        chat_tx,
        raw_tx,
    });

    // 0. Version-check before sending anything else.
    let mut control_stream = crate::spawn::ensure_executor().await?;
    crate::spawn::check_version(&mut control_stream).await?;

    // 1. Connect the control socket and hand it to a permanent reader
    //    before sending `create_session`, so the create response and any
    //    events that follow it are never dropped from a transient
    //    `BufReader`.
    let (control_read_half, mut control_write_half) = control_stream.into_split();
    let mut control_reader = BufReader::new(control_read_half);

    let create_request = serde_json::json!({"op": "create_session"});
    socket::write_json_line(&mut control_write_half, &create_request).await?;
    let create_resp = match socket::read_json_line(&mut control_reader).await? {
        Some(v) => v,
        None => return Err("executor closed connection during create_session".to_string()),
    };
    let (session_id, home_run_id) = parse_session_setup(&create_resp)?;

    let control = Connection::new_control(control_reader, control_write_half, Arc::clone(&out));

    // 2. Connect the fresh session socket and hand it a permanent reader.
    let state_root = gremlins::config::state_root();
    let session_stream = socket::connect_session_socket(&state_root, &session_id).await?;
    let (session_read_half, session_write_half) = session_stream.into_split();
    let session_reader = BufReader::new(session_read_half);
    let session = Connection::new_session(
        session_reader,
        session_write_half,
        session_id,
        home_run_id,
        Arc::clone(&out),
    );

    Ok((
        DaemonClient {
            control,
            session,
            out,
            session_recreate: Mutex::new(()),
            #[cfg(test)]
            aux_streams: Mutex::new(None),
        },
        event_rx,
        chat_rx,
        raw_rx,
    ))
}

/// Background reader: deserialise every incoming line from one connection.
///
/// Unsolicited [`EventKind`] values are classified first and forwarded
/// to the TUI event loop. Everything else is treated as a response to the
/// oldest pending request (FIFO). If no request is waiting, the line is
/// forwarded as a raw JSON value.
async fn read_loop(
    mut reader: BufReader<OwnedReadHalf>,
    out: Arc<OutState>,
    responses: Arc<Mutex<VecDeque<oneshot::Sender<Value>>>>,
) {
    loop {
        let value = match socket::read_json_line(&mut reader).await {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => break,
        };

        // Classify event_lagged notifications first — they don't
        // deserialize as EventKind and would otherwise be misrouted
        // to a pending response waiter, corrupting FIFO alignment.
        if value.get("type").and_then(|v| v.as_str()) == Some("event_lagged") {
            let _ = out.raw_tx.send(value);
            continue;
        }

        // Classify as an EventKind next — unsolicited events can arrive
        // at any time, even when a request is pending.
        if let Ok(event) = serde_json::from_value::<EventKind>(value.clone()) {
            // Chat events carry a `chat_turn` tag inserted by the daemon's
            // chat_payload wrapper. Extract it before forwarding so the TUI
            // can filter stale turns.
            //
            // IMPORTANT: only classify as chat when the tag is present.
            // Bare `type:"error"` lines (request/response errors) must reach
            // the FIFO response queue, not the chat stream.
            if event.is_chat_event() {
                let chat_turn = value
                    .get("chat_turn")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let can_route_to_chat = |kind: &EventKind| -> bool {
                    match kind {
                        EventKind::Error { .. } | EventKind::SystemWarning { .. } => {
                            chat_turn.is_some()
                        }
                        _ => true,
                    }
                };
                if can_route_to_chat(&event) {
                    let execution =
                        if matches!(event, EventKind::TurnStarted { .. }) && chat_turn.is_some() {
                            Execution::from_frame(&value)
                        } else {
                            None
                        };
                    let _ = out.chat_tx.send((event, chat_turn, execution));
                } else {
                    // Bare error/system_warning — treat as a response.
                    let response_tx = {
                        let mut guard = responses.lock().await;
                        guard.pop_front()
                    };
                    if let Some(tx) = response_tx {
                        let _ = tx.send(value);
                    } else {
                        let _ = out.raw_tx.send(value);
                    }
                }
            } else {
                let _ = out.event_tx.send(event);
            }
            continue;
        }

        // Not an event — deliver as a response to the oldest pending request.
        let response_tx = {
            let mut guard = responses.lock().await;
            guard.pop_front()
        };

        if let Some(tx) = response_tx {
            let _ = tx.send(value);
            continue;
        }

        // Forward as a raw line (log_line, status, etc.).
        let _ = out.raw_tx.send(value);
    }

    // Drop all pending response senders so waiters see "connection
    // closed" instead of hanging forever.
    responses.lock().await.clear();
}

#[cfg(test)]
mod run_follow_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    fn ack() -> Value {
        json!({"type":"attached", "scope":"run", "id":"run", "capabilities":["observe", "replay_checkpoint"]})
    }

    #[tokio::test]
    async fn waits_for_ack_and_sends_only_passive_attach() {
        let (client, peer) = UnixStream::pair().unwrap();
        let mut peer = BufReader::new(peer);
        let mut pending = tokio::spawn(follow_run(client, "owner", "run"));
        assert_eq!(
            socket::read_json_line(&mut peer).await.unwrap().unwrap(),
            json!({"op":"attach", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
        );
        assert!(timeout(Duration::from_millis(20), &mut pending)
            .await
            .is_err());
        socket::write_json_line(peer.get_mut(), &ack())
            .await
            .unwrap();
        let (mut rx, handle) = timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(peer);
        handle.await.unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn rejects_errors_eof_malformed_and_wrong_acknowledgments() {
        let mut cases = vec![
            (
                Some(json!({"type":"error", "message":"gremlin run is not running"}).to_string()),
                "gremlin run is not running",
            ),
            (None, "closed connection"),
            (Some("not json".to_owned()), ""),
            (Some(json!({"type":"ready"}).to_string()), "unexpected"),
        ];
        for field in ["type", "scope", "id"] {
            let mut wrong = ack();
            wrong[field] = json!("wrong");
            cases.push((Some(wrong.to_string()), "unexpected"));
        }
        for (response, expected) in cases {
            let (client, peer) = UnixStream::pair().unwrap();
            let server = tokio::spawn(async move {
                let mut peer = BufReader::new(peer);
                socket::read_json_line(&mut peer).await.unwrap().unwrap();
                if let Some(response) = response {
                    peer.get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .unwrap();
                }
            });
            let error = timeout(Duration::from_secs(1), follow_run(client, "owner", "run"))
                .await
                .unwrap()
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_missing_replay_checkpoint_capability_without_waiting_for_frames() {
        for capabilities in [
            None,
            Some(json!(["observe"])),
            Some(json!("replay_checkpoint")),
        ] {
            let (client, peer) = UnixStream::pair().unwrap();
            let mut peer = BufReader::new(peer);
            let pending = tokio::spawn(follow_run(client, "owner", "run"));
            socket::read_json_line(&mut peer).await.unwrap().unwrap();
            let mut response = ack();
            response.as_object_mut().unwrap().remove("capabilities");
            if let Some(capabilities) = capabilities {
                response["capabilities"] = capabilities;
            }
            socket::write_json_line(peer.get_mut(), &response)
                .await
                .unwrap();
            // Keep the socket open and send no checkpoint: negotiation must fail immediately.
            let error = timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.contains("replay_checkpoint"), "{error}");
            assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        }
    }

    /// Exercise the persisted-history/socket boundary, not an artificial mpsc
    /// snapshot. The peer stays open until after reconciliation so EOF cannot
    /// accidentally make collection appear to work.
    async fn persisted_socket_handoff(blob: &str, persisted: &str, frames: Vec<Value>) {
        use crate::tui::watch_replay::{collect_checkpoint, decode_history};
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
        struct Transcript(std::path::PathBuf);
        impl Drop for Transcript {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let transcript = Transcript(std::env::temp_dir().join(format!(
            "gremlins-follow-{}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
        )));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&transcript.0)
            .unwrap();
        let expected_persisted = persisted.to_owned();
        let persisted = persisted.to_owned();
        let outgoing = frames.clone();
        let future = frames[0].clone();
        let future_outgoing = future.clone();
        let (append_tx, append_rx) = oneshot::channel();
        let (persisted_tx, persisted_rx) = oneshot::channel();
        let (collect_tx, collect_rx) = oneshot::channel();
        let (future_tx, future_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel();
        let (client, peer) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            assert_eq!(
                socket::read_json_line(&mut peer).await.unwrap().unwrap(),
                json!({"op":"attach", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
            );
            socket::write_json_line(peer.get_mut(), &ack())
                .await
                .unwrap();
            append_rx.await.unwrap();
            file.write_all(persisted.as_bytes()).unwrap();
            file.flush().unwrap();
            persisted_tx.send(()).unwrap();
            collect_rx.await.unwrap();
            // Let collection start with an empty receiver. Frames are still on
            // the socket side of the handoff, unlike a prefilled rx.len() test.
            tokio::time::sleep(Duration::from_millis(150)).await;
            for frame in outgoing {
                socket::write_json_line(peer.get_mut(), &frame)
                    .await
                    .unwrap();
            }
            socket::write_json_line(
                peer.get_mut(),
                &json!({"type":"replay_checkpoint", "id":"run", "scope":"run"}),
            )
            .await
            .unwrap();
            future_rx.await.unwrap();
            socket::write_json_line(peer.get_mut(), &future_outgoing)
                .await
                .unwrap();
            close_rx.await.unwrap();
        });
        let (mut rx, reader) = timeout(Duration::from_secs(2), follow_run(client, "owner", "run"))
            .await
            .unwrap()
            .unwrap();
        append_tx.send(()).unwrap();
        timeout(Duration::from_secs(2), persisted_rx)
            .await
            .unwrap()
            .unwrap();
        let lines = std::fs::read_to_string(&transcript.0)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let history = decode_history(lines, blob, "run");
        let expected_history = if blob.starts_with("artifacts/") {
            vec![json!({"type":"stream_chunk", "text":"hello world"})]
        } else {
            vec![
                json!({"type":"exec_output", "id":"run", "is_stderr":false, "text":"hello", "ts":""}),
                json!({"type":"exec_output", "id":"run", "is_stderr":false, "text":" world", "ts":""}),
            ]
        };
        assert_eq!(history, expected_history);
        assert!(rx.is_empty(), "server has not sent any live frames yet");
        collect_tx.send(()).unwrap();
        let snapshot = timeout(Duration::from_secs(2), collect_checkpoint(&mut rx))
            .await
            .unwrap();
        assert!(snapshot.fallback.is_none());
        let buffered = snapshot.values;
        assert_eq!(
            buffered, frames,
            "collection must include delayed socket frames"
        );
        // Persistence is not evidence that a native frame is covered.
        // Preserve every delayed frame, even when disk contains matching text.
        assert_eq!(buffered, frames);
        assert_eq!(
            std::fs::read_to_string(&transcript.0).unwrap(),
            expected_persisted
        );

        // Identical output emitted after the finite handoff is a new event.
        future_tx.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), rx.recv()).await.unwrap(),
            Some(future)
        );
        close_tx.send(()).unwrap();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), reader)
            .await
            .unwrap()
            .unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn persisted_assistant_aggregate_does_not_suppress_delayed_socket_chunks() {
        let record = json!({"type":"assistant", "message":{"content":[
            {"type":"text", "text":"hello world"}
        ]}});
        persisted_socket_handoff(
            "artifacts/stream-agent.jsonl",
            &format!("{record}\n"),
            vec![
                json!({"type":"stream_chunk", "text":"hello"}),
                json!({"type":"log_line", "id":"run", "line":"operational log"}),
                json!({"type":"stream_chunk", "text":" world"}),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn persisted_combined_exec_does_not_suppress_stdout_and_stderr_socket_frames() {
        persisted_socket_handoff(
            "command_logs/exec-build.log",
            "hello\n world\n",
            vec![
                json!({"type":"exec_output", "id":"run", "is_stderr":false, "text":"hello", "ts":"now"}),
                json!({"type":"log_line", "id":"run", "line":"operational log"}),
                json!({"type":"exec_output", "id":"run", "is_stderr":true, "text":" world", "ts":"later"}),
            ],
        ).await;
    }

    #[tokio::test]
    async fn forwards_native_frames_unchanged_including_buffered_frames_after_ack() {
        let frames = vec![
            json!({"type":"stream_chunk", "text":"answer"}),
            json!({"type":"reasoning_chunk", "text":"thinking"}),
            json!({"type":"exec_output", "id":"run", "is_stderr":false, "text":"stdout", "ts":"now"}),
            json!({"type":"exec_output", "id":"run", "is_stderr":true, "text":"stderr", "ts":"now"}),
            json!({"type":"log_line", "id":"run", "line":"log"}),
            json!({"type":"stage_boundary", "id":"run", "stage":"exec", "stage_type":"exec", "active":true}),
            json!({"type":"stage_transition", "id":"run", "stage":"review"}),
            json!({"type":"stream_reset"}),
            json!({"type":"lagged", "skipped":2}),
            json!({"type":"run_completed", "id":"run", "exit_code":0}),
        ];
        let (client, peer) = UnixStream::pair().unwrap();
        let outgoing = frames.clone();
        let server = tokio::spawn(async move {
            let mut peer = BufReader::new(peer);
            socket::read_json_line(&mut peer).await.unwrap().unwrap();
            let payload = std::iter::once(ack())
                .chain(outgoing)
                .map(|frame| format!("{frame}\n"))
                .collect::<String>();
            peer.get_mut().write_all(payload.as_bytes()).await.unwrap();
        });
        let (mut rx, handle) = follow_run(client, "owner", "run").await.unwrap();
        assert_eq!(rx.max_capacity(), 256);
        for frame in frames {
            assert_eq!(
                timeout(Duration::from_secs(1), rx.recv()).await.unwrap(),
                Some(frame)
            );
        }
        assert!(timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .is_none());
        handle.await.unwrap();
        server.await.unwrap();
    }
}

#[cfg(test)]
mod replay_fallback_tests {
    use super::*;
    use crate::tui::watch_replay::replay_and_dedup;
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    async fn read(reader: &mut BufReader<UnixStream>) -> Value {
        timeout(Duration::from_secs(2), socket::read_json_line(reader))
            .await
            .expect("socket request timed out")
            .unwrap()
            .expect("socket closed unexpectedly")
    }

    async fn output(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("replay output timed out")
            .expect("replay output closed")
    }

    fn unwrap_line(frame: Value) -> Value {
        assert_eq!(frame["type"], "view_log");
        assert_eq!(frame["id"], "run");
        assert_eq!(frame["generation"], 73);
        serde_json::from_str(frame["line"].as_str().unwrap()).unwrap()
    }

    async fn fallback(history_error: bool) {
        let (native, native_peer) = UnixStream::pair().unwrap();
        let (log, log_peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![native, log],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let replay = tokio::spawn(replay_and_dedup(
            Arc::clone(&client),
            "run".into(),
            String::new(),
            String::new(),
            73,
            tx,
        ));
        let mut native = BufReader::new(native_peer);
        assert_eq!(
            read(&mut native).await,
            json!({"op":"attach", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
        );
        socket::write_json_line(
            native.get_mut(),
            &json!({"type":"error", "message":"run attachment unsupported"}),
        )
        .await
        .unwrap();
        let mut log = BufReader::new(log_peer);
        let mut session = BufReader::new(session);
        let request = timeout(Duration::from_secs(2), async {
            tokio::select! {
                biased;
                request = socket::read_json_line(&mut log) => request.unwrap().unwrap(),
                request = socket::read_json_line(&mut session) => panic!("history requested before log subscription: {request:?}"),
            }
        }).await.expect("fallback did not subscribe");
        assert_eq!(
            request,
            json!({"op":"log", "id":"run", "follow":true, "session_id":"owner"})
        );
        // The endpoint owns both persisted and live operational history.
        // A separate disk read would duplicate the persisted portion.
        if history_error {
            socket::write_json_line(
                log.get_mut(),
                &json!({"type":"error", "message":"permission denied: original diagnostic"}),
            )
            .await
            .unwrap();
        } else {
            for line in [
                "[history-ts] repeated",
                "[history-ts] repeated",
                "plain history",
            ] {
                socket::write_json_line(
                    log.get_mut(),
                    &json!({"type":"log_line", "id":"run", "line":line}),
                )
                .await
                .unwrap();
            }
        }
        let diagnostic = unwrap_line(output(&mut rx).await);
        assert!(diagnostic["line"]
            .as_str()
            .unwrap()
            .contains("native run follow unavailable"));
        if history_error {
            let error = unwrap_line(output(&mut rx).await);
            assert_eq!(error["type"], "log_line");
            assert_eq!(error["line"], "permission denied: original diagnostic");
        } else {
            for text in ["repeated", "repeated", "plain history"] {
                assert_eq!(
                    unwrap_line(output(&mut rx).await),
                    json!({"type":"log_line", "id":"run", "line":text})
                );
            }
        }
        // Identical content after the persisted output is a new live event.
        socket::write_json_line(
            log.get_mut(),
            &json!({"type":"log_line", "id":"run", "line":"[live-ts] repeated"}),
        )
        .await
        .unwrap();
        assert_eq!(
            unwrap_line(output(&mut rx).await),
            json!({"type":"log_line", "id":"run", "line":"repeated"})
        );
        assert!(
            timeout(
                Duration::from_millis(100),
                socket::read_json_line(&mut session)
            )
            .await
            .is_err(),
            "operational fallback must not request disk history"
        );
        assert!(rx.try_recv().is_err(), "unexpected duplicate replay output");
        assert!(!replay.is_finished(), "live subscription ended prematurely");

        // Cancellation must abort the auxiliary reader, not leave its socket open.
        replay.abort();
        assert!(replay.await.unwrap_err().is_cancelled());
        assert_eq!(
            timeout(Duration::from_secs(2), socket::read_json_line(&mut log))
                .await
                .expect("auxiliary reader leaked after cancellation")
                .unwrap(),
            None
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn native_replay_forwards_lifecycle_and_live_frames_but_not_checkpoints() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (client, _peers) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![stream],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let replay = tokio::spawn(replay_and_dedup(
            Arc::clone(&client),
            "run".into(),
            String::new(),
            String::new(),
            73,
            tx,
        ));
        let mut peer = BufReader::new(peer);
        assert_eq!(read(&mut peer).await["op"], "attach");
        let frames = [
            json!({"type":"stage_boundary", "id":"run", "stage":"build"}),
            json!({"type":"stream_chunk", "text":"same"}),
            json!({"type":"stream_chunk", "text":"same"}),
        ];
        let mut wire = format!(
            "{}\n",
            json!({"type":"attached", "scope":"run", "id":"run", "capabilities":["observe", "replay_checkpoint"]})
        );
        for frame in &frames {
            wire.push_str(&format!("{frame}\n"));
        }
        wire.push_str("{\"type\":\"replay_checkpoint\",\"complete\":true}\n");
        use tokio::io::AsyncWriteExt;
        peer.get_mut().write_all(wire.as_bytes()).await.unwrap();
        for frame in frames {
            assert_eq!(unwrap_line(output(&mut rx).await), frame);
        }
        let live = json!({"type":"stage_completed", "id":"run", "stage":"build"});
        socket::write_json_line(peer.get_mut(), &live)
            .await
            .unwrap();
        assert_eq!(unwrap_line(output(&mut rx).await), live);
        assert!(rx.try_recv().is_err(), "checkpoint should not be forwarded");
        replay.abort();
        assert!(replay.await.unwrap_err().is_cancelled());
        assert_eq!(
            timeout(Duration::from_secs(2), socket::read_json_line(&mut peer))
                .await
                .expect("native reader leaked after cancellation")
                .unwrap(),
            None
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn native_gap_preserves_disk_and_all_original_agent_chunks() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (replacement, replacement_peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![stream, replacement],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let replay = tokio::spawn(replay_and_dedup(
            Arc::clone(&client),
            "run".into(),
            "agent".into(),
            "agent".into(),
            73,
            tx,
        ));
        let mut peer = BufReader::new(peer);
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"attach", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
        );
        let operational = json!({"type":"log_line", "id":"run", "line":"operational log"});
        for frame in [
            json!({"type":"attached", "scope":"run", "id":"run", "capabilities":["observe", "replay_checkpoint"]}),
            json!({"type":"history_gap", "id":"run"}),
            json!({"type":"stream_chunk", "text":"hello"}),
            operational.clone(),
            json!({"type":"stream_chunk", "text":" world"}),
            json!({"type":"replay_checkpoint", "complete":false}),
        ] {
            socket::write_json_line(peer.get_mut(), &frame)
                .await
                .unwrap();
        }
        let mut session = BufReader::new(session);
        assert_eq!(
            read(&mut session).await,
            json!({"op":"read_definition", "id":"run", "session_id":"owner"})
        );
        socket::write_json_line(
            session.get_mut(),
            &json!({"stages":[{"name":"agent", "type":"agent"}]}),
        )
        .await
        .unwrap();
        assert_eq!(
            read(&mut session).await,
            json!({"op":"read_blob", "id":"run", "blob":"artifacts/stream-agent.jsonl", "tail":500, "session_id":"owner"})
        );
        // Persistence can lead delivery beyond the checkpoint while disk
        // history is being read. Keep this emission as well as the disk copy.
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"stream_chunk", "text":" buffered"}),
        )
        .await
        .unwrap();
        let history = ["older", "hello world buffered"].map(|text| {
            json!({"type":"assistant", "message":{"content":[{"type":"text", "text":text}]}})
                .to_string()
        });
        socket::write_json_line(session.get_mut(), &json!({"lines":history}))
            .await
            .unwrap();

        let diagnostic = unwrap_line(output(&mut rx).await);
        assert_eq!(diagnostic["type"], "log_line");
        assert!(diagnostic["line"].as_str().unwrap().contains("history_gap"));
        let overlap = unwrap_line(output(&mut rx).await);
        assert!(overlap["line"]
            .as_str()
            .unwrap()
            .contains("possible overlap"));
        assert!(overlap["line"]
            .as_str()
            .unwrap()
            .contains("not execution-scoped"));
        for text in ["older", "hello world buffered"] {
            assert_eq!(
                unwrap_line(output(&mut rx).await),
                json!({"type":"stream_chunk", "text":text})
            );
        }
        let gap = unwrap_line(output(&mut rx).await);
        assert_eq!(gap["type"], "log_line");
        assert!(gap["line"].as_str().unwrap().contains("history_gap"));
        assert_eq!(
            unwrap_line(output(&mut rx).await),
            json!({"type":"stream_chunk", "text":"hello"})
        );
        assert_eq!(unwrap_line(output(&mut rx).await), operational);
        assert_eq!(
            unwrap_line(output(&mut rx).await),
            json!({"type":"stream_chunk", "text":" world"})
        );
        assert_eq!(
            unwrap_line(output(&mut rx).await),
            json!({"type":"stream_chunk", "text":" buffered"})
        );
        let mut replacement = BufReader::new(replacement_peer);
        assert!(
            timeout(
                Duration::from_millis(100),
                socket::read_json_line(&mut replacement)
            )
            .await
            .is_err(),
            "must not open a replacement subscription"
        );
        // Once replay output is observed, an identical live chunk is not overlap.
        let live = json!({"type":"stream_chunk", "text":"hello"});
        socket::write_json_line(peer.get_mut(), &live)
            .await
            .unwrap();
        assert_eq!(unwrap_line(output(&mut rx).await), live);
        assert!(
            rx.try_recv().is_err(),
            "checkpoint or duplicate output leaked"
        );
        assert!(!replay.is_finished());
        replay.abort();
        assert!(replay.await.unwrap_err().is_cancelled());
        assert_eq!(
            timeout(Duration::from_secs(2), socket::read_json_line(&mut peer))
                .await
                .expect("native reader leaked after cancellation")
                .unwrap(),
            None
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn native_gap_preserves_disk_and_repeated_original_exec_emissions() {
        for (disk_count, third_before_response) in [(2, false), (2, true), (3, true)] {
            let (original, original_peer) = UnixStream::pair().unwrap();
            let (replacement, replacement_peer) = UnixStream::pair().unwrap();
            let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "home_run_id":"home"}),
                vec![original, replacement],
            );
            let client = Arc::new(client);
            let (tx, mut rx) = mpsc::unbounded_channel();
            let replay = tokio::spawn(replay_and_dedup(
                Arc::clone(&client),
                "run".into(),
                "build".into(),
                "build".into(),
                73,
                tx,
            ));
            let mut original = BufReader::new(original_peer);
            let attach = json!({"op":"attach", "scope":"run", "id":"run", "session_id":"owner", "replay":true});
            assert_eq!(read(&mut original).await, attach);
            let exec = json!({"type":"exec_output", "id":"run", "is_stderr":false,
                "ts":"now", "text":"same", "sequence":2});
            let mut third = exec.clone();
            third["sequence"] = json!(3);
            // The first emission was evicted; only the second is retained.
            for frame in [
                json!({"type":"attached", "scope":"run", "id":"run", "capabilities":["observe", "replay_checkpoint"]}),
                json!({"type":"history_gap", "id":"run"}),
                exec.clone(),
                json!({"type":"replay_checkpoint", "complete":false}),
            ] {
                socket::write_json_line(original.get_mut(), &frame)
                    .await
                    .unwrap();
            }
            let mut session = BufReader::new(session);
            assert_eq!(
                read(&mut session).await,
                json!({"op":"read_definition", "id":"run", "session_id":"owner"})
            );
            socket::write_json_line(
                session.get_mut(),
                &json!({"stages":[{"name":"build", "type":"exec"}]}),
            )
            .await
            .unwrap();
            assert_eq!(
                read(&mut session).await,
                json!({"op":"read_blob", "id":"run", "blob":"command_logs/exec-build.log", "tail":500, "session_id":"owner"})
            );
            if third_before_response {
                socket::write_json_line(original.get_mut(), &third)
                    .await
                    .unwrap();
                // Queue the original emission while the disk response is pending.
                tokio::task::yield_now().await;
            }
            socket::write_json_line(
                session.get_mut(),
                &json!({"lines":vec!["same"; disk_count], "through_sequence":disk_count,
                    "covered_sequences": (1..=disk_count).collect::<Vec<_>>()}),
            )
            .await
            .unwrap();
            let mut replacement = BufReader::new(replacement_peer);
            assert!(
                timeout(
                    Duration::from_millis(100),
                    socket::read_json_line(&mut replacement)
                )
                .await
                .is_err(),
                "must not open replacement subscription"
            );
            if !third_before_response {
                socket::write_json_line(original.get_mut(), &third)
                    .await
                    .unwrap();
            }
            // A barrier proves original frames continue without replacement traffic.
            let barrier = json!({"type":"stage_completed", "id":"run", "stage":"build"});
            socket::write_json_line(original.get_mut(), &barrier)
                .await
                .unwrap();
            let mut count = 0;
            loop {
                let frame = unwrap_line(output(&mut rx).await);
                if frame == barrier {
                    break;
                }
                match frame["type"].as_str().unwrap() {
                    "exec_output" => {
                        assert_eq!(frame["text"], "same");
                        count += 1;
                    }
                    "log_line" => assert!(
                        frame["line"].as_str().unwrap().contains("history_gap")
                            || frame["line"].as_str().unwrap().contains("possible overlap")
                    ),
                    other => panic!("unexpected replay frame: {other}"),
                }
            }
            assert_eq!(
                count, disk_count + 2,
                "disk and both original emissions must remain; third before response: {third_before_response}"
            );
            assert!(
                !replay.is_finished(),
                "original subscription ended prematurely"
            );
            assert!(rx.try_recv().is_err());
            replay.abort();
            assert!(replay.await.unwrap_err().is_cancelled());
            assert_eq!(
                timeout(
                    Duration::from_secs(2),
                    socket::read_json_line(&mut original)
                )
                .await
                .expect("original reader leaked after cancellation")
                .unwrap(),
                None
            );
            client.shutdown().await;
        }
    }

    #[tokio::test]
    async fn native_gap_does_not_trust_stage_b_coverage_to_suppress_original_output() {
        let (original, original_peer) = UnixStream::pair().unwrap();
        let (replacement, replacement_peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "home_run_id":"home"}),
            vec![original, replacement],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let replay = tokio::spawn(replay_and_dedup(
            Arc::clone(&client),
            "run".into(),
            "B".into(),
            "B".into(),
            73,
            tx,
        ));
        let attach =
            json!({"op":"attach", "scope":"run", "id":"run", "session_id":"owner", "replay":true});
        let mut original = BufReader::new(original_peer);
        assert_eq!(read(&mut original).await, attach);
        let stage_a = json!({"type":"exec_output", "id":"run",
            "is_stderr":false, "ts":"now", "text":"same", "sequence":10});
        let stage_b = json!({"type":"exec_output", "id":"run",
            "is_stderr":false, "ts":"now", "text":"same", "sequence":20});
        for frame in [
            json!({"type":"attached", "scope":"run", "id":"run", "capabilities":["observe", "replay_checkpoint"]}),
            json!({"type":"history_gap", "id":"run"}),
            stage_a.clone(),
            stage_b.clone(),
            json!({"type":"replay_checkpoint", "complete":false}),
        ] {
            socket::write_json_line(original.get_mut(), &frame)
                .await
                .unwrap();
        }
        let mut session = BufReader::new(session);
        assert_eq!(
            read(&mut session).await,
            json!({"op":"read_definition", "id":"run", "session_id":"owner"})
        );
        socket::write_json_line(
            session.get_mut(),
            &json!({"stages":[{"name":"A", "type":"exec"}, {"name":"B", "type":"exec"}]}),
        )
        .await
        .unwrap();
        assert_eq!(
            read(&mut session).await,
            json!({"op":"read_blob", "id":"run", "blob":"command_logs/exec-B.log", "tail":500, "session_id":"owner"})
        );
        socket::write_json_line(
            session.get_mut(),
            &json!({"lines":["same"], "through_sequence":20, "covered_sequences":[20]}),
        )
        .await
        .unwrap();
        let mut replacement = BufReader::new(replacement_peer);
        assert!(
            timeout(
                Duration::from_millis(100),
                socket::read_json_line(&mut replacement)
            )
            .await
            .is_err(),
            "must not open replacement subscription"
        );
        // A barrier proves both stages reach the pane without needing more output.
        let barrier = json!({"type":"stage_completed", "id":"run", "stage":"B"});
        socket::write_json_line(original.get_mut(), &barrier)
            .await
            .unwrap();
        let mut pane = Vec::new();
        loop {
            let frame = unwrap_line(output(&mut rx).await);
            if frame == barrier {
                break;
            }
            if frame["type"] == "log_line" {
                assert!(
                    frame["line"].as_str().unwrap().contains("history_gap")
                        || frame["line"].as_str().unwrap().contains("possible overlap")
                );
            } else {
                pane.push(frame);
            }
        }
        assert_eq!(
            pane,
            vec![
                json!({"type":"exec_output", "id":"run", "is_stderr":false, "text":"same", "ts":""}),
                stage_a,
                stage_b.clone(),
            ],
            "disk B and both original frames must remain, despite identical text"
        );

        // Legacy coverage metadata does not establish exact raw-log coverage.
        let mut unrelated = stage_b.clone();
        unrelated["sequence"] = json!(15);
        let mut later = stage_b.clone();
        later["sequence"] = json!(30);
        for frame in [&stage_b, &unrelated, &later, &barrier] {
            socket::write_json_line(original.get_mut(), frame)
                .await
                .unwrap();
        }
        assert_eq!(unwrap_line(output(&mut rx).await), stage_b);
        assert_eq!(unwrap_line(output(&mut rx).await), unrelated);
        assert_eq!(unwrap_line(output(&mut rx).await), later);
        assert_eq!(unwrap_line(output(&mut rx).await), barrier);
        assert!(
            rx.try_recv().is_err(),
            "unexpected extra output after all original frames"
        );
        assert!(
            !replay.is_finished(),
            "original subscription ended prematurely"
        );
        replay.abort();
        assert!(replay.await.unwrap_err().is_cancelled());
        assert_eq!(
            timeout(
                Duration::from_secs(2),
                socket::read_json_line(&mut original)
            )
            .await
            .expect("original reader leaked after cancellation")
            .unwrap(),
            None
        );
        client.shutdown().await;
    }

    #[tokio::test]
    async fn rejected_run_uses_log_endpoint_for_persisted_and_live_history() {
        fallback(false).await;
    }

    #[tokio::test]
    async fn rejected_run_surfaces_endpoint_error_and_keeps_live_subscription() {
        fallback(true).await;
    }
}

#[cfg(test)]
mod session_identity_tests {
    use super::*;

    #[test]
    fn setup_requires_authoritative_home_identity() {
        assert_eq!(
            parse_session_setup(&json!({"session_id": "s", "home_run_id": "reserved-42"})),
            Ok(("s".into(), "reserved-42".into()))
        );
        for home in [Value::Null, json!(42)] {
            assert_eq!(
                parse_session_setup(&json!({"session_id": "s", "home_run_id": home})),
                Err("create_session response missing 'home_run_id'".into())
            );
        }
        assert!(parse_session_setup(&json!({"session_id": "s"})).is_err());
        assert!(parse_session_setup(&json!({"home_run_id": "reserved-42"})).is_err());
        assert_eq!(
            parse_session_setup(&json!({"type": "error", "message": "reservation failed"})),
            Err("reservation failed".into())
        );
    }

    #[tokio::test]
    async fn restart_replaces_socket_and_both_identities() {
        let (client, _peers) = DaemonClient::fake_with_identity(json!({
            "session_id": "old-session", "home_run_id": "reserved-old"
        }));
        assert_eq!(client.session_id().await, "old-session");
        assert_eq!(client.home_run_id().await, "reserved-old");
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = stream.into_split();
        let (session_id, home_run_id) = parse_session_setup(&json!({
            "session_id": "new-session", "home_run_id": "registry-new"
        }))
        .unwrap();
        client
            .session
            .restart(BufReader::new(read), write, session_id, home_run_id)
            .await;
        assert_eq!(client.session_id().await, "new-session");
        assert_eq!(client.home_run_id().await, "registry-new");
        assert_eq!(client.session_generation().await, 1);
        assert!(client
            .send_session_request_for_generation(json!({"op": "status"}), 0)
            .await
            .is_err());

        let server = async move {
            let mut peer = BufReader::new(peer);
            let request = socket::read_json_line(&mut peer).await.unwrap().unwrap();
            assert_eq!(request["session_id"], "new-session");
            socket::write_json_line(peer.get_mut(), &json!({"ok": true}))
                .await
                .unwrap();
        };
        let (response, ()) =
            tokio::join!(client.send_session_request(json!({"op": "status"})), server);
        assert_eq!(response.unwrap(), json!({"ok": true}));
        client.shutdown().await;
    }
}

#[cfg(test)]
mod correction_tests {
    use super::*;
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn turn_start_preserves_the_owner_turn_and_execution_as_one_event() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        let (reader, _writer) = client.into_split();
        let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
        let out = Arc::new(OutState {
            event_tx: mpsc::unbounded_channel().0,
            chat_tx,
            raw_tx: mpsc::unbounded_channel().0,
        });
        let reader = tokio::spawn(read_loop(BufReader::new(reader), out, Arc::default()));
        socket::write_json_line(&mut peer, &json!({"type":"turn_started", "chat_turn":"owner-turn", "id":"home", "execution_id":"exec-home", "nonce":23})).await.unwrap();
        drop(peer);
        reader.await.unwrap();
        let (kind, turn, target) = chat_rx.recv().await.unwrap();
        assert!(matches!(kind, EventKind::TurnStarted { .. }));
        assert_eq!(turn.as_deref(), Some("owner-turn"));
        assert_eq!(
            target,
            Some(Execution {
                run_id: "home".into(),
                execution_id: "exec-home".into(),
                nonce: 23
            })
        );
        assert!(chat_rx.recv().await.is_none());
    }

    fn execution() -> Execution {
        Execution {
            run_id: "pipeline".into(),
            execution_id: "exec-stage".into(),
            nonce: 9,
        }
    }

    fn frame(kind: &str) -> Value {
        json!({"type":kind,"id":"pipeline","nonce":9,"scope":"conversation","execution_id":"exec-stage"})
    }

    fn attached() -> Value {
        let mut ack = frame("attached");
        ack["capabilities"] = json!(["observe", "message"]);
        ack
    }

    /// Drive the actual auxiliary socket; assert exactly one undecorated
    /// message (or none if the handshake failed), then EOF, with no retries.
    async fn exchange(ack: Value, expect_message: bool, replies: Vec<Value>) -> Result<(), String> {
        let (client, server) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            let mut peer = BufReader::new(server);
            let attach = socket::read_json_line(&mut peer).await.unwrap().unwrap();
            assert_eq!(
                attach,
                json!({"op":"attach", "scope":"conversation", "mode":"control", "id":"pipeline", "execution_id":"exec-stage", "nonce":9,"session_id":"owner"})
            );
            socket::write_json_line(peer.get_mut(), &ack).await.unwrap();
            if expect_message {
                let cmd = socket::read_json_line(&mut peer).await.unwrap().unwrap();
                assert_eq!(
                    cmd,
                    json!({"op":"message", "text":"  stop here\n", "req_id":"17"})
                );
            }
            for reply in replies {
                socket::write_json_line(peer.get_mut(), &reply)
                    .await
                    .unwrap();
            }
            use tokio::io::AsyncWriteExt;
            let _ = peer.get_mut().shutdown().await;
            assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        });
        let result = match attach_conversation(client, "owner", Selector::Exact(&execution())).await
        {
            Ok((stream, bound)) => send_correction(stream, &bound, 17, "  stop here\n").await,
            Err(reason) => Err(reason),
        };
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn correction_receipt_ignores_scoped_display_and_correlates_submission() {
        let mut delivered = frame("delivered");
        delivered["req_id"] = json!("17");
        exchange(
            attached(),
            true,
            vec![frame("operator_message"), frame("error"), delivered.clone()],
        )
        .await
        .unwrap();
        for (field, wrong) in [
            ("req_id", json!("16")),
            ("execution_id", json!("next")),
            ("nonce", json!(10)),
            ("id", json!("home")),
        ] {
            let mut foreign = delivered.clone();
            foreign[field] = wrong;
            let err = exchange(attached(), true, vec![foreign]).await.unwrap_err();
            assert!(err.contains("uncertain"), "{err}");
        }
    }

    #[tokio::test]
    async fn stale_and_read_only_attachments_receive_no_message() {
        for (field, wrong) in [
            ("capabilities", json!(["observe"])),
            ("execution_id", json!("replacement")),
            ("id", json!("foreign")),
            ("nonce", json!(11)),
        ] {
            let mut ack = attached();
            ack[field] = wrong;
            assert!(exchange(ack, false, vec![]).await.is_err());
        }
    }

    #[tokio::test]
    async fn correction_dropped_busy_and_unconfirmed_disconnect_are_not_success() {
        let mut dropped = frame("correction_dropped");
        dropped["req_id"] = json!("17");
        assert!(exchange(attached(), true, vec![dropped])
            .await
            .unwrap_err()
            .contains("not delivered"));
        assert_eq!(
            exchange(
                attached(),
                true,
                vec![json!({"type":"error","message":"busy"})]
            )
            .await
            .unwrap_err(),
            "busy"
        );
        assert!(exchange(attached(), true, vec![])
            .await
            .unwrap_err()
            .contains("uncertain"));
    }
}
