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

use std::collections::VecDeque;
use std::sync::Arc;

use gremlins::executor::socket;
use gremlins::executor::DaemonEvent;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex};

use super::chat::{parse_chat_event, ChatEvent};

/// Outbound side of a single daemon socket connection.
struct OutState {
    /// Daemon events forwarded to the TUI event loop.
    event_tx: mpsc::UnboundedSender<DaemonEvent>,
    /// Chat events routed to the TUI chat arm.
    chat_tx: mpsc::UnboundedSender<ChatEvent>,
    /// Raw JSON lines that are neither responses nor DaemonEvents.
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
        Self::new(reader, write_half, None, out)
    }

    fn new_session(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        session_id: String,
        out: Arc<OutState>,
    ) -> Self {
        Self::new(reader, write_half, Some(session_id), out)
    }

    fn new(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        session_id: Option<String>,
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
}

impl DaemonClient {
    /// The current session id, atomically paired with the session socket in
    /// [`Connection::write`].
    pub async fn session_id(&self) -> String {
        self.session.current_session_id().await
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
        let state_root = gremlins::config::state_root();
        socket::connect_session_socket(&state_root, &self.session_id().await).await
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
        if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
            return Err(resp
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string());
        }
        let new_session_id = resp
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or("create_session response missing 'session_id'")?
            .to_string();

        let state_root = gremlins::config::state_root();
        let stream = socket::connect_session_socket(&state_root, &new_session_id).await?;
        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);
        self.session
            .restart(reader, write_half, new_session_id.clone())
            .await;
        Ok(())
    }

    /// Abort both background read loops, closing the sockets.
    pub async fn shutdown(&self) {
        self.control.shutdown().await;
        self.session.shutdown().await;
    }

    /// A sender for chat events, so a caller can inject a locally-detected
    /// error (e.g. a failed chat request) into the same stream the TUI's
    /// chat arm consumes.
    pub fn chat_sender(&self) -> mpsc::UnboundedSender<ChatEvent> {
        self.out.chat_tx.clone()
    }
}

/// Connect to the executor daemon, create a session, and return the client
/// handle plus receivers for unsolicited [`DaemonEvent`] values,
/// [`ChatEvent`] values, and raw JSON lines.
pub async fn connect() -> Result<
    (
        DaemonClient,
        mpsc::UnboundedReceiver<DaemonEvent>,
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

    // 1. Connect the control socket and hand it to a permanent reader
    //    before sending `create_session`, so the create response and any
    //    events that follow it are never dropped from a transient
    //    `BufReader`.
    let control_stream = crate::spawn::ensure_executor().await?;
    let (control_read_half, mut control_write_half) = control_stream.into_split();
    let mut control_reader = BufReader::new(control_read_half);

    let create_request = serde_json::json!({"op": "create_session"});
    socket::write_json_line(&mut control_write_half, &create_request).await?;
    let create_resp = match socket::read_json_line(&mut control_reader).await? {
        Some(v) => v,
        None => return Err("executor closed connection during create_session".to_string()),
    };
    if create_resp.get("type").and_then(|v| v.as_str()) == Some("error") {
        return Err(create_resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_string());
    }
    let session_id = create_resp
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("create_session response missing 'session_id'")?
        .to_string();

    let control = Connection::new_control(control_reader, control_write_half, Arc::clone(&out));

    // 2. Connect the fresh session socket and hand it a permanent reader.
    let state_root = gremlins::config::state_root();
    let session_stream = socket::connect_session_socket(&state_root, &session_id).await?;
    let (session_read_half, session_write_half) = session_stream.into_split();
    let session_reader = BufReader::new(session_read_half);
    let session = Connection::new_session(
        session_reader,
        session_write_half,
        session_id.clone(),
        Arc::clone(&out),
    );

    Ok((
        DaemonClient {
            control,
            session,
            out,
            session_recreate: Mutex::new(()),
        },
        event_rx,
        chat_rx,
        raw_rx,
    ))
}

/// Background reader: deserialise every incoming line from one connection.
///
/// Unsolicited [`DaemonEvent`] values are classified first and forwarded
/// to the TUI event loop. Chat events are classified next. Everything else
/// is treated as a response to the oldest pending request (FIFO). If no
/// request is waiting, the line is forwarded as a raw JSON value.
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
        // deserialize as DaemonEvent and would otherwise be misrouted
        // to a pending response waiter, corrupting FIFO alignment.
        if value.get("type").and_then(|v| v.as_str()) == Some("event_lagged") {
            let _ = out.raw_tx.send(value);
            continue;
        }

        // Classify as a DaemonEvent next — unsolicited events can arrive
        // at any time, even when a request is pending.
        if let Ok(event) = serde_json::from_value::<DaemonEvent>(value.clone()) {
            let _ = out.event_tx.send(event);
            continue;
        }

        // Classify chat events before FIFO response delivery. Chat turns
        // stream as unsolicited JSON lines and must not be misrouted to a
        // pending request waiter (which would corrupt FIFO alignment).
        if let Some(event) = parse_chat_event(&value) {
            let _ = out.chat_tx.send(event);
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
