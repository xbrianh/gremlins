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
//! Daemon broadcasts use the control socket. SessionChat requests are ack-only;
//! a dedicated passive bus watch supplies display events. Each request socket
//! owns its own response FIFO and background reader.

mod session_chat;

#[cfg(test)]
#[path = "client/session_chat_shutdown_tests.rs"]
mod session_chat_shutdown_tests;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use super::protocol::Execution;
use gremlins::executor::socket;
use gremlins_magic_bus::EventKind;
use serde_json::{json, Value};

use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex};

/// A session_chat event, its wire turn tag, and its exact agent execution when advertised.
pub(crate) type ChatEvent = (EventKind, Option<String>, Option<Execution>);

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
    write: Arc<Mutex<WriteState>>,
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
    /// Registry-reserved session_chat identity belonging to the same session socket.
    chat_run_id: Option<String>,
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
        Self::new(
            reader,
            write_half,
            None,
            None,
            out,
            #[cfg(test)]
            false,
        )
    }

    fn new_session(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        session_id: String,
        chat_run_id: String,
        out: Arc<OutState>,
    ) -> Self {
        Self::new(
            reader,
            write_half,
            Some(session_id),
            Some(chat_run_id),
            out,
            #[cfg(test)]
            false,
        )
    }

    fn new(
        reader: BufReader<OwnedReadHalf>,
        write_half: OwnedWriteHalf,
        session_id: Option<String>,
        chat_run_id: Option<String>,
        out: Arc<OutState>,
        #[cfg(test)] retain_write_until_reader_stops: bool,
    ) -> Self {
        let responses: Arc<Mutex<VecDeque<oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let write = Arc::new(Mutex::new(WriteState {
            write_half,
            session_id,
            chat_run_id,
            generation: 0,
        }));
        let read_out = Arc::clone(&out);
        let read_responses = Arc::clone(&responses);
        // Only the shutdown regression fake retains the writer: its peer's EOF
        // must prove the reader stopped, rather than merely that the client dropped.
        #[cfg(test)]
        let retained_write = retain_write_until_reader_stops.then(|| Arc::clone(&write));
        let read_handle = tokio::spawn(async move {
            #[cfg(test)]
            let _retained_write = retained_write;
            read_loop(reader, read_out, read_responses).await;
        });

        Self {
            write,
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
        new_chat_run_id: String,
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
        write.chat_run_id = Some(new_chat_run_id);
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

struct SelectedInput {
    input: Arc<crate::input::InputClient>,
    epoch: u64,
}

type SelectedInputs = HashMap<(String, String, u64), SelectedInput>;

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
    session_chat_display: Mutex<Option<session_chat::Display>>,
    selected_inputs: StdMutex<SelectedInputs>,
    input_epoch: AtomicU64,
    #[cfg(test)]
    aux_streams: Arc<Mutex<Option<VecDeque<tokio::net::UnixStream>>>>,
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
        let (client, peers, _, _, _) = Self::fake_with_aux_streams_and_channels(response, streams);
        (client, peers)
    }

    #[cfg(test)]
    pub(super) fn fake_with_aux_streams_and_channels(
        response: Value,
        streams: Vec<tokio::net::UnixStream>,
    ) -> (
        Self,
        [tokio::net::UnixStream; 2],
        mpsc::UnboundedReceiver<EventKind>,
        mpsc::UnboundedReceiver<ChatEvent>,
        mpsc::UnboundedReceiver<Value>,
    ) {
        Self::fake_with_reader_lifetime(response, streams, false)
    }

    #[cfg(test)]
    pub(super) fn fake_with_reader_lifetime(
        response: Value,
        streams: Vec<tokio::net::UnixStream>,
        retain_write_until_reader_stops: bool,
    ) -> (
        Self,
        [tokio::net::UnixStream; 2],
        mpsc::UnboundedReceiver<EventKind>,
        mpsc::UnboundedReceiver<ChatEvent>,
        mpsc::UnboundedReceiver<Value>,
    ) {
        let (session_id, chat_run_id) = parse_session_setup(&response).unwrap();
        let (control, control_peer) = tokio::net::UnixStream::pair().unwrap();
        let (session, session_peer) = tokio::net::UnixStream::pair().unwrap();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (chat_tx, chat_rx) = mpsc::unbounded_channel();
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let out = Arc::new(OutState {
            event_tx,
            chat_tx,
            raw_tx,
        });
        let (control_read, control_write) = control.into_split();
        let (session_read, session_write) = session.into_split();
        (
            Self {
                control: Connection::new(
                    BufReader::new(control_read),
                    control_write,
                    None,
                    None,
                    Arc::clone(&out),
                    retain_write_until_reader_stops,
                ),
                session: Connection::new(
                    BufReader::new(session_read),
                    session_write,
                    Some(session_id),
                    Some(chat_run_id),
                    Arc::clone(&out),
                    retain_write_until_reader_stops,
                ),
                out,
                session_recreate: Mutex::new(()),
                session_chat_display: Mutex::new(None),
                selected_inputs: StdMutex::new(HashMap::new()),
                input_epoch: AtomicU64::new(0),
                aux_streams: Arc::new(Mutex::new(Some(streams.into()))),
            },
            [control_peer, session_peer],
            event_rx,
            chat_rx,
            raw_rx,
        )
    }

    /// The current session id, atomically paired with the session socket in
    /// [`Connection::write`].
    pub async fn session_id(&self) -> String {
        self.session.current_session_id().await
    }

    /// The registry-reserved session_chat run id paired with the current session socket.
    pub async fn chat_run_id(&self) -> String {
        self.session
            .write
            .lock()
            .await
            .chat_run_id
            .clone()
            .expect("session connection has a session_chat run id")
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
        let _guard = self.session_recreate.lock().await;
        self.send_session_chat_or_session(request, None).await
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
        self.send_session_chat_or_session(request, Some(expected_generation))
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

    /// Send exactly one correction on a socket pinned to the operator's execution.
    /// This connection never forwards display frames to either TUI event stream.
    pub async fn deliver_correction(
        &self,
        execution: &Execution,
        id: u64,
        text: &str,
    ) -> Result<(), String> {
        self.selected_input(
            execution,
            json!({"cmd":"message", "req_id":id.to_string(), "text":text}),
        )
        .await
    }

    /// Send native input to exactly this execution. Queued is not delivery.
    /// Cloning the handle before waiting lets controls unblock a pending message.
    pub async fn selected_input(
        &self,
        execution: &Execution,
        command: Value,
    ) -> Result<(), String> {
        self.selected_input_at(self.input_epoch(), execution, command)
            .await
    }

    pub fn input_epoch(&self) -> u64 {
        self.input_epoch.load(Ordering::SeqCst)
    }

    pub async fn selected_input_at(
        &self,
        epoch: u64,
        execution: &Execution,
        command: Value,
    ) -> Result<(), String> {
        let key = (
            execution.run_id.clone(),
            execution.stage_run_id.clone(),
            execution.nonce,
        );
        let input = {
            let _session_guard = self.session_recreate.lock().await;
            let cached = {
                let mut inputs = self.selected_inputs.lock().unwrap();
                if self.input_epoch.load(Ordering::SeqCst) != epoch {
                    return Err("input selection invalidated".into());
                }
                if inputs
                    .get(&key)
                    .is_some_and(|selected| selected.input.is_failed())
                {
                    inputs.remove(&key);
                }
                inputs.get_mut(&key).map(|selected| {
                    selected.epoch = epoch;
                    Arc::clone(&selected.input)
                })
            };
            if let Some(input) = cached {
                input
            } else {
                let req_id = command["req_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or("input requires nonempty string 'req_id'")?;
                let session = self.session_id().await;
                let stream = self.open_aux_stream().await?;
                let input = Arc::new(
                    crate::input::InputClient::bind(
                        stream,
                        execution.input_request(&session, req_id),
                    )
                    .await?,
                );
                let mut inputs = self.selected_inputs.lock().unwrap();
                if self.input_epoch.load(Ordering::SeqCst) != epoch {
                    return Err("input selection invalidated".into());
                }
                inputs.insert(
                    key.clone(),
                    SelectedInput {
                        input: Arc::clone(&input),
                        epoch,
                    },
                );
                input
            }
        };
        let receipt = {
            let _inputs = self.selected_inputs.lock().unwrap();
            if self.input_epoch.load(Ordering::SeqCst) != epoch {
                return Err("input selection invalidated".into());
            }
            // Registration and selection release are serialized; waiting is not.
            input.send(command)
        };
        let result = receipt.await;
        let mut inputs = self.selected_inputs.lock().unwrap();
        if inputs.get(&key).is_some_and(|selected| {
            selected.epoch != self.input_epoch.load(Ordering::SeqCst)
                && Arc::ptr_eq(&selected.input, &input)
                && !input.has_pending()
        }) {
            inputs.remove(&key);
        }
        result
    }

    /// Release idle handles, retaining bindings needed to unblock queued input.
    pub fn release_inputs(&self) {
        let mut inputs = self.selected_inputs.lock().unwrap();
        self.input_epoch.fetch_add(1, Ordering::SeqCst);
        inputs.retain(|_, selected| selected.input.has_pending());
    }

    fn invalidate_inputs(&self) {
        let mut inputs = self.selected_inputs.lock().unwrap();
        self.input_epoch.fetch_add(1, Ordering::SeqCst);
        for selected in inputs.values() {
            selected.input.close();
        }
        inputs.clear();
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
    pub async fn follow_run(
        &self,
        id: &str,
    ) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String> {
        self.follow_watch(id, None, None).await
    }

    /// Observe a run or an exact stage invocation without acquiring conversation control.
    pub async fn follow_watch(
        &self,
        run_id: &str,
        stage: Option<&str>,
        stage_run_id: Option<&str>,
    ) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String> {
        let session_id = self.session_id().await;
        let stream = self.open_aux_stream().await?;
        follow_watch(stream, &session_id, run_id, stage, stage_run_id, true).await
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

        self.invalidate_inputs();
        self.session_chat_display.lock().await.take();
        let close_request = serde_json::json!({"op": "close_session"});
        let _ = self.session.send_on_session(close_request).await?;

        let create_request = serde_json::json!({"op": "create_session"});
        let resp = self.control.send(create_request).await?;
        let (new_session_id, new_chat_run_id) = parse_session_setup(&resp)?;

        let state_root = gremlins::config::state_root();
        let stream = socket::connect_session_socket(&state_root, &new_session_id).await?;
        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);
        self.session
            .restart(reader, write_half, new_session_id, new_chat_run_id)
            .await;
        Ok(())
    }

    /// Retire pending session_chat acquisitions and abort the background readers.
    pub async fn shutdown(&self) {
        {
            // Match install_session_chat's lock order so a late watch acknowledgment
            // cannot publish a display after shutdown, even if none exists yet.
            let mut display = self.session_chat_display.lock().await;
            let mut write = self.session.write.lock().await;
            write.generation += 1;
            display.take();
        }
        self.invalidate_inputs();
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

async fn follow_run(
    stream: tokio::net::UnixStream,
    session_id: &str,
    id: &str,
) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String> {
    follow_watch(stream, session_id, id, None, None, true).await
}

async fn follow_watch(
    stream: tokio::net::UnixStream,
    session_id: &str,
    id: &str,
    stage: Option<&str>,
    stage_run_id: Option<&str>,
    replay: bool,
) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String> {
    follow_watch_mapped(stream, session_id, id, stage, stage_run_id, replay, Some).await
}

async fn follow_watch_mapped<F>(
    mut stream: tokio::net::UnixStream,
    session_id: &str,
    id: &str,
    stage: Option<&str>,
    stage_run_id: Option<&str>,
    replay: bool,
    mut receive: F,
) -> Result<(mpsc::Receiver<Value>, tokio::task::JoinHandle<()>), String>
where
    F: FnMut(Value) -> Option<Value> + Send + 'static,
{
    let mut request =
        json!({"op":"watch", "scope":"run", "id":id, "session_id":session_id, "replay":replay});
    for (key, value) in [("stage", stage), ("stage_run_id", stage_run_id)] {
        if let Some(value) = value {
            request[key] = json!(value);
        }
    }
    socket::write_json_line(&mut stream, &request).await?;
    let mut reader = BufReader::new(stream);
    let ack = socket::read_json_line(&mut reader)
        .await?
        .ok_or("executor closed connection before watch acknowledgment")?;
    if ack["type"] == "error" {
        return Err(ack["message"].as_str().unwrap_or("watch failed").to_owned());
    }
    if ack["type"] != "watched"
        || ack["scope"] != "run"
        || ack["id"] != id
        || ack["replay"] != replay
        || ack.get("stage") != Some(&json!(stage))
        || ack.get("stage_run_id") != Some(&json!(stage_run_id))
    {
        return Err(format!("unexpected watch acknowledgment: {ack}"));
    }

    let (tx, rx) = mpsc::channel(256);
    let handle = tokio::spawn(async move {
        while let Some(frame) = socket::read_json_line(&mut reader).await.ok().flatten() {
            let Some(frame) = receive(frame) else {
                continue;
            };
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
    Ok((required("session_id")?, required("chat_run_id")?))
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
    let (session_id, chat_run_id) = parse_session_setup(&create_resp)?;

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
        chat_run_id,
        Arc::clone(&out),
    );

    Ok((
        DaemonClient {
            control,
            session,
            out,
            session_recreate: Mutex::new(()),
            session_chat_display: Mutex::new(None),
            selected_inputs: StdMutex::new(HashMap::new()),
            input_epoch: AtomicU64::new(0),
            #[cfg(test)]
            aux_streams: Arc::new(Mutex::new(None)),
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
                    // SessionChat display comes exclusively from its passive bus watch.
                    // Retire legacy frames without consuming a response waiter.
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
        json!({"type":"watched", "scope":"run", "id":"run", "replay":true, "stage":null, "stage_run_id":null})
    }

    #[tokio::test]
    async fn waits_for_ack_and_sends_only_passive_watch() {
        let (client, peer) = UnixStream::pair().unwrap();
        let mut peer = BufReader::new(peer);
        let mut pending = tokio::spawn(follow_run(client, "owner", "run"));
        assert_eq!(
            socket::read_json_line(&mut peer).await.unwrap().unwrap(),
            json!({"op":"watch", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
        );
        assert!(timeout(Duration::from_millis(20), &mut pending)
            .await
            .is_err());
        let frame = json!({"type":"stream_chunk", "text":"answer"});
        let payload = format!("{}\n{frame}\n", ack());
        peer.get_mut().write_all(payload.as_bytes()).await.unwrap();
        let (mut rx, handle) = timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), rx.recv()).await.unwrap(),
            Some(frame)
        );
        assert!(
            timeout(Duration::from_millis(20), socket::read_json_line(&mut peer))
                .await
                .is_err(),
            "passive attachment must not send commands"
        );
        drop(peer);
        timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
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
            let mut peer = BufReader::new(peer);
            let pending = tokio::spawn(follow_run(client, "owner", "run"));
            timeout(Duration::from_secs(1), socket::read_json_line(&mut peer))
                .await
                .expect("passive attach request was not sent")
                .unwrap()
                .unwrap();
            if let Some(response) = response {
                peer.get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                // A rejected response must fail without waiting for EOF.
                let error = timeout(Duration::from_secs(1), pending)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert!(error.contains(expected), "{error}");
                drop(peer);
            } else {
                // Only the EOF case closes the peer before awaiting the result.
                drop(peer);
                let error = timeout(Duration::from_secs(1), pending)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert!(error.contains(expected), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn rejects_missing_or_mismatched_selector_fields_without_waiting_for_frames() {
        for (field, value) in [
            ("replay", None),
            ("replay", Some(json!(false))),
            ("replay", Some(json!("true"))),
            ("stage", None),
            ("stage", Some(json!("build"))),
            ("stage_run_id", None),
            ("stage_run_id", Some(json!("invocation"))),
            ("type", Some(json!("attached"))),
            ("scope", Some(json!("conversation"))),
        ] {
            let (client, peer) = UnixStream::pair().unwrap();
            let mut peer = BufReader::new(peer);
            let pending = tokio::spawn(follow_run(client, "owner", "run"));
            socket::read_json_line(&mut peer).await.unwrap().unwrap();
            let mut response = ack();
            response.as_object_mut().unwrap().remove(field);
            if let Some(value) = value {
                response[field] = value;
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
            assert!(error.contains("unexpected watch acknowledgment"), "{error}");
            assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn exact_watch_uses_only_auxiliary_current_session_and_rejects_wrong_identity() {
        for wrong_field in [None, Some("stage"), Some("stage_run_id")] {
            let (stream, peer) = UnixStream::pair().unwrap();
            let (client, [control, session]) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"current-owner", "chat_run_id":"session_chat"}),
                vec![stream],
            );
            let client = Arc::new(client);
            let watcher = Arc::clone(&client);
            let pending = tokio::spawn(async move {
                watcher
                    .follow_watch("run", Some("build"), Some("invocation"))
                    .await
            });
            let mut peer = BufReader::new(peer);
            assert_eq!(
                socket::read_json_line(&mut peer).await.unwrap().unwrap(),
                json!({"op":"watch", "scope":"run", "id":"run",
                    "session_id":"current-owner", "replay":true,
                    "stage":"build", "stage_run_id":"invocation"})
            );
            let mut response = ack();
            response["stage"] = json!("build");
            response["stage_run_id"] = json!("invocation");
            if let Some(field) = wrong_field {
                response[field] = json!("other");
            }
            let frame = json!({"type":"replay_checkpoint", "complete":true});
            peer.get_mut()
                .write_all(format!("{response}\n{frame}\n").as_bytes())
                .await
                .unwrap();
            let result = timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap();
            if wrong_field.is_some() {
                assert!(result
                    .unwrap_err()
                    .contains("unexpected watch acknowledgment"));
            } else {
                let (mut rx, handle) = result.unwrap();
                assert_eq!(
                    timeout(Duration::from_secs(1), rx.recv()).await.unwrap(),
                    Some(frame)
                );
                handle.abort();
                let _ = handle.await;
            }
            let mut control = BufReader::new(control);
            let mut session = BufReader::new(session);
            assert!(timeout(Duration::from_millis(20), async {
                tokio::select! {
                    request = socket::read_json_line(&mut control) => panic!("watch used control: {request:?}"),
                    request = socket::read_json_line(&mut session) => panic!("watch used primary session: {request:?}"),
                }
            }).await.is_err());
            client.shutdown().await;
        }
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
            json!({"type":"history_gap", "id":"run", "reason":"retention"}),
            json!({"type":"replay_checkpoint", "id":"run", "complete":false}),
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
        let (replacement, replacement_peer) = UnixStream::pair().unwrap();
        let (client, [control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "chat_run_id":"session_chat"}),
            vec![native, replacement],
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
            json!({"op":"watch", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
        );
        socket::write_json_line(
            native.get_mut(),
            &json!({"type":"error", "message":"run attachment unsupported"}),
        )
        .await
        .unwrap();
        let mut replacement = BufReader::new(replacement_peer);
        let mut session = BufReader::new(session);
        let mut control = BufReader::new(control);
        let request = timeout(Duration::from_secs(2), async {
            tokio::select! {
                request = socket::read_json_line(&mut session) => request.unwrap().unwrap(),
                request = socket::read_json_line(&mut replacement) => panic!("fallback opened a replacement follow: {request:?}"),
            }
        }).await.expect("fallback did not request bounded disk history");
        assert_eq!(
            request,
            json!({"op":"read_blob", "id":"run", "blob":"log", "tail":500, "session_id":"owner"})
        );
        // Rejection permits only a bounded persisted tail, never a replacement follow.
        if history_error {
            socket::write_json_line(
                session.get_mut(),
                &json!({"type":"error", "message":"permission denied: original diagnostic"}),
            )
            .await
            .unwrap();
        } else {
            socket::write_json_line(
                session.get_mut(),
                &json!({"type":"blob", "lines":[
                    "[history-ts] repeated", "[history-ts] repeated", "plain history"
                ]}),
            )
            .await
            .unwrap();
        }
        let frame = output(&mut rx).await;
        assert!(frame.get("native").is_none());
        let diagnostic = unwrap_line(frame);
        assert!(diagnostic["line"]
            .as_str()
            .unwrap()
            .contains("native run follow unavailable"));
        assert!(diagnostic["line"]
            .as_str()
            .unwrap()
            .contains("bounded persisted tail only (no live follow)"));
        if history_error {
            let frame = output(&mut rx).await;
            assert!(frame.get("native").is_none());
            let error = unwrap_line(frame);
            assert_eq!(error["type"], "log_line");
            assert_eq!(
                error["line"],
                "history unavailable (log): permission denied: original diagnostic"
            );
        } else {
            let warning = output(&mut rx).await;
            assert!(warning.get("native").is_none());
            assert_eq!(
                unwrap_line(warning),
                json!({"type":"log_line", "id":"run",
                "line":"history discontinuity: bounded persisted tail (up to 500 lines); persisted tail and original stream may overlap; possible overlap is preserved, not deduplicated"})
            );
            for text in ["repeated", "repeated", "plain history"] {
                assert_eq!(
                    {
                        let frame = output(&mut rx).await;
                        assert!(frame.get("native").is_none());
                        unwrap_line(frame)
                    },
                    json!({"type":"log_line", "id":"run", "line":text})
                );
            }
        }
        assert_eq!(
            output(&mut rx).await,
            json!({"type":"view_error", "id":"run",
            "generation":73, "force":true,
            "message":"native run follow unavailable; showing bounded persisted tail only, no live updates"})
        );
        timeout(Duration::from_secs(2), replay)
            .await
            .unwrap()
            .unwrap();
        assert!(rx.recv().await.is_none(), "unexpected extra replay output");
        assert!(socket::read_json_line(&mut native).await.unwrap().is_none());
        assert!(timeout(Duration::from_millis(100), async {
            tokio::select! {
                request = socket::read_json_line(&mut control) => panic!("unexpected shared control request: {request:?}"),
                request = socket::read_json_line(&mut session) => panic!("extra disk request: {request:?}"),
                request = socket::read_json_line(&mut replacement) => panic!("replacement follow: {request:?}"),
            }
        }).await.is_err());
        client.shutdown().await;
    }

    struct Disk {
        definition: Value,
        request: Value,
        response: Value,
    }

    struct NativeCase {
        stage: &'static str,
        frames: Vec<Value>,
        disk: Option<Disk>,
        before_response: Vec<Value>,
        after_response: Vec<Value>,
        expected: Vec<Value>,
        live: Vec<Value>,
    }

    async fn native_handoff(mut case: NativeCase) {
        let checkpoint = case.frames.last().unwrap().clone();
        assert_eq!(checkpoint["type"], "replay_checkpoint");
        let checkpoint_index =
            case.expected.len() - case.before_response.len() - case.after_response.len();
        case.expected.insert(checkpoint_index, checkpoint);
        let native_start = case.expected.len()
            - case.frames.len()
            - case.before_response.len()
            - case.after_response.len();
        let native_frames: Vec<_> = case
            .frames
            .iter()
            .chain(&case.before_response)
            .chain(&case.after_response)
            .cloned()
            .collect();
        let (stream, peer) = UnixStream::pair().unwrap();
        let (replacement, replacement_peer) = UnixStream::pair().unwrap();
        let (client, [control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "chat_run_id":"session_chat"}),
            vec![stream, replacement],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let replay = tokio::spawn(replay_and_dedup(
            Arc::clone(&client),
            "run".into(),
            case.stage.into(),
            case.stage.into(),
            73,
            tx,
        ));
        let mut peer = BufReader::new(peer);
        let mut session = BufReader::new(session);
        let mut control = BufReader::new(control);
        let mut replacement = BufReader::new(replacement_peer);
        assert_eq!(
            read(&mut peer).await,
            json!({"op":"watch", "scope":"run", "id":"run", "session_id":"owner", "replay":true})
        );
        // One write also exercises frames buffered alongside the attach acknowledgment.
        use tokio::io::AsyncWriteExt;
        let mut wire = format!(
            "{}\n",
            json!({"type":"watched", "scope":"run", "id":"run",
            "replay":true, "stage":null, "stage_run_id":null})
        );
        for frame in &case.frames {
            wire.push_str(&format!("{frame}\n"));
        }
        peer.get_mut().write_all(wire.as_bytes()).await.unwrap();
        if let Some(disk) = case.disk {
            assert_eq!(
                read(&mut session).await,
                json!({"op":"read_definition", "id":"run", "session_id":"owner"})
            );
            socket::write_json_line(session.get_mut(), &disk.definition)
                .await
                .unwrap();
            assert_eq!(read(&mut session).await, disk.request);
            // Never send these until read_blob is pending; persistence may lead delivery.
            for frame in &case.before_response {
                socket::write_json_line(peer.get_mut(), frame)
                    .await
                    .unwrap();
            }
            tokio::task::yield_now().await;
            socket::write_json_line(session.get_mut(), &disk.response)
                .await
                .unwrap();
        } else {
            assert!(case.before_response.is_empty());
        }
        for frame in &case.after_response {
            socket::write_json_line(peer.get_mut(), frame)
                .await
                .unwrap();
        }
        let watched =
            json!({"type":"watched", "id":"run", "replay":true, "stage":null, "stage_run_id":null});
        let wrapper = output(&mut rx).await;
        assert_eq!(wrapper["native"], watched);
        assert_eq!(unwrap_line(wrapper), watched);
        for (index, expected) in case.expected.into_iter().enumerate() {
            let wrapper = output(&mut rx).await;
            if index < native_start {
                assert!(
                    wrapper.get("native").is_none(),
                    "disk history must not establish liveness"
                );
            } else {
                assert_eq!(wrapper["native"], native_frames[index - native_start]);
            }
            let actual = unwrap_line(wrapper);
            if expected["line"] == "history_gap" {
                assert_eq!(actual["type"], "log_line");
                assert_eq!(actual["id"], "run");
                assert!(actual["line"].as_str().unwrap().contains("history_gap"));
            } else {
                assert_eq!(actual, expected);
            }
        }
        // These are new original-socket emissions, even if byte-identical to replay.
        for frame in case.live {
            socket::write_json_line(peer.get_mut(), &frame)
                .await
                .unwrap();
            let wrapper = output(&mut rx).await;
            assert_eq!(wrapper["native"], frame);
            assert_eq!(unwrap_line(wrapper), frame);
        }
        assert!(timeout(Duration::from_millis(100), async {
            tokio::select! {
                request = socket::read_json_line(&mut control) => panic!("unexpected shared control request: {request:?}"),
                request = socket::read_json_line(&mut session) => panic!("unexpected disk read: {request:?}"),
                request = socket::read_json_line(&mut replacement) => panic!("replacement attach: {request:?}"),
                frame = rx.recv() => panic!("checkpoint or extra output: {frame:?}"),
            }
        }).await.is_err());
        assert!(
            !replay.is_finished(),
            "original subscription ended prematurely"
        );
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
    async fn native_replay_handoff_cases() {
        let chunk = |text: &str| json!({"type":"stream_chunk", "text":text});
        let log = |text: &str| json!({"type":"log_line", "id":"run", "line":text});
        let exec = |sequence, stderr| {
            json!({"type":"exec_output", "id":"run",
            "is_stderr":stderr, "ts":"now", "text":"same", "sequence":sequence})
        };
        let disk_exec = json!({"type":"exec_output", "id":"run",
            "is_stderr":false, "ts":"", "text":"same"});
        let checkpoint = |complete| json!({"type":"replay_checkpoint", "complete":complete});
        let gap = json!({"type":"history_gap", "id":"run"});
        // Exact diagnostics and their position are part of the handoff contract.
        let warnings = vec![
            log("history_gap: native replay history is incomplete"),
            log("history discontinuity: bounded persisted tail (up to 500 lines); persisted tail and original stream may overlap; possible overlap is preserved, not deduplicated; stage history is not execution-scoped and may include other executions"),
        ];
        // Match the diagnostic at this position without coupling to JSON formatting.
        let gap_log = log("history_gap");
        let disk = |stage: &str, kind: &str, blob: &str, response| Disk {
            definition: json!({"stages":[{"name":"A", "type":"exec"}, {"name":stage, "type":kind}]}),
            request: json!({"op":"read_blob", "id":"run", "blob":blob, "tail":500, "session_id":"owner"}),
            response,
        };
        let boundary = json!({"type":"stage_boundary", "id":"run", "stage":"build"});
        let build_completed = json!({"type":"stage_completed", "id":"run", "stage":"build"});
        let mut cases = vec![NativeCase {
            stage: "",
            frames: vec![
                boundary.clone(),
                chunk("same"),
                chunk("same"),
                checkpoint(true),
            ],
            disk: None,
            before_response: vec![],
            after_response: vec![],
            expected: vec![boundary, chunk("same"), chunk("same")],
            live: vec![build_completed.clone()],
        }];
        let mut expected = warnings.clone();
        expected.extend([
            chunk("older"),
            chunk("hellohello world buffered"),
            gap_log.clone(),
            chunk("hello"),
            chunk("hello"),
            json!({"type":"reasoning_chunk", "text":"same"}),
            log("operational log"),
            chunk(" world"),
            chunk(" buffered"),
        ]);
        cases.push(NativeCase {
            stage: "agent",
            frames: vec![gap.clone(), chunk("hello"), chunk("hello"),
                json!({"type":"reasoning_chunk", "text":"same"}),
                log("operational log"), chunk(" world"), checkpoint(false)],
            disk: Some(disk("agent", "agent", "artifacts/stream-agent.jsonl", json!({
                "lines": (["older", "hellohello world buffered"].map(|text|
                    json!({"type":"assistant", "message":{"content":[{"type":"text", "text":text}]}}).to_string()))
            }))),
            before_response: vec![chunk(" buffered")],
            after_response: vec![],
            expected,
            live: vec![chunk("hello"), json!({"type":"reasoning_chunk", "text":"same"})],
        });
        for (disk_count, third_before_response) in [(2, false), (2, true), (3, true)] {
            let third = exec(3, false);
            let mut expected = warnings.clone();
            expected.extend(vec![disk_exec.clone(); disk_count]);
            expected.extend([
                gap_log.clone(),
                exec(2, false),
                exec(2, true),
                log("same"),
                log("same"),
                third.clone(),
                build_completed.clone(),
            ]);
            cases.push(NativeCase {
                stage: "build",
                // Repeated text across stdout, stderr, and logs must retain type and order.
                frames: vec![
                    gap.clone(),
                    exec(2, false),
                    exec(2, true),
                    log("same"),
                    log("same"),
                    checkpoint(false),
                ],
                disk: Some(disk(
                    "build",
                    "exec",
                    "command_logs/exec-build.log",
                    json!({
                        "lines":vec!["same"; disk_count], "through_sequence":disk_count,
                        "covered_sequences":(1..=disk_count).collect::<Vec<_>>()
                    }),
                )),
                before_response: if third_before_response {
                    vec![third.clone()]
                } else {
                    vec![]
                },
                after_response: if third_before_response {
                    vec![build_completed.clone()]
                } else {
                    vec![third, build_completed.clone()]
                },
                expected,
                live: vec![exec(2, false), exec(2, true), log("same")],
            });
        }
        let stage_b_completed = json!({"type":"stage_completed", "id":"run", "stage":"B"});
        let stage_a = exec(10, false);
        let stage_b = exec(20, false);
        let mut expected = warnings;
        expected.extend([
            disk_exec,
            gap_log,
            stage_a.clone(),
            stage_b.clone(),
            stage_b_completed.clone(),
        ]);
        cases.push(NativeCase {
            stage: "B",
            frames: vec![gap, stage_a, stage_b.clone(), checkpoint(false)],
            disk: Some(disk(
                "B",
                "exec",
                "command_logs/exec-B.log",
                json!({
                    "lines":["same"], "through_sequence":20, "covered_sequences":[20]
                }),
            )),
            before_response: vec![],
            after_response: vec![stage_b_completed.clone()],
            expected,
            live: vec![stage_b, exec(15, false), exec(30, false), stage_b_completed],
        });
        for case in cases {
            native_handoff(case).await;
        }
    }

    #[tokio::test]
    async fn rejected_run_reads_only_bounded_disk_history() {
        fallback(false).await;
    }

    #[tokio::test]
    async fn rejected_run_surfaces_disk_error_without_live_subscription() {
        fallback(true).await;
    }
}

#[cfg(test)]
mod session_identity_tests {
    use super::*;

    #[test]
    fn setup_requires_authoritative_session_chat_identity() {
        assert_eq!(
            parse_session_setup(&json!({"session_id": "s", "chat_run_id": "reserved-42"})),
            Ok(("s".into(), "reserved-42".into()))
        );
        for session_chat in [Value::Null, json!(42)] {
            assert_eq!(
                parse_session_setup(&json!({"session_id": "s", "chat_run_id": session_chat})),
                Err("create_session response missing 'chat_run_id'".into())
            );
        }
        assert!(parse_session_setup(&json!({"session_id": "s"})).is_err());
        assert!(parse_session_setup(&json!({"chat_run_id": "reserved-42"})).is_err());
        assert_eq!(
            parse_session_setup(&json!({"type": "error", "message": "reservation failed"})),
            Err("reservation failed".into())
        );
    }

    #[tokio::test]
    async fn restart_replaces_socket_and_both_identities() {
        let (client, _peers) = DaemonClient::fake_with_identity(json!({
            "session_id": "old-session", "chat_run_id": "reserved-old"
        }));
        assert_eq!(client.session_id().await, "old-session");
        assert_eq!(client.chat_run_id().await, "reserved-old");
        let (stream, peer) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = stream.into_split();
        let (session_id, chat_run_id) = parse_session_setup(&json!({
            "session_id": "new-session", "chat_run_id": "registry-new"
        }))
        .unwrap();
        client
            .session
            .restart(BufReader::new(read), write, session_id, chat_run_id)
            .await;
        assert_eq!(client.session_id().await, "new-session");
        assert_eq!(client.chat_run_id().await, "registry-new");
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
    async fn unsolicited_turn_start_is_retired_without_display() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        let (reader, _writer) = client.into_split();
        let (chat_tx, mut chat_rx) = mpsc::unbounded_channel();
        let out = Arc::new(OutState {
            event_tx: mpsc::unbounded_channel().0,
            chat_tx,
            raw_tx: mpsc::unbounded_channel().0,
        });
        let reader = tokio::spawn(read_loop(BufReader::new(reader), out, Arc::default()));
        socket::write_json_line(&mut peer, &json!({"type":"turn_started", "chat_turn":"owner-turn", "id":"session_chat", "stage_run_id":"exec-session_chat", "nonce":23})).await.unwrap();
        drop(peer);
        reader.await.unwrap();
        assert!(chat_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn stale_epoch_before_first_poll_rejects_without_auxiliary_bind() {
        use tokio::time::{timeout, Duration};

        timeout(Duration::from_secs(3), async {
            let execution = Execution {
                run_id: "pipeline".into(),
                stage_run_id: "exec-stage".into(),
                nonce: 9,
            };
            let (stream, peer) = UnixStream::pair().unwrap();
            let (client, _peers) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                vec![stream],
            );
            let epoch = client.input_epoch();
            let pending =
                client.selected_input_at(epoch, &execution, json!({"cmd":"pause", "req_id":"17"}));
            client.release_inputs();
            assert_eq!(pending.await.unwrap_err(), "input selection invalidated");
            assert!(client.selected_inputs.lock().unwrap().is_empty());
            assert_eq!(client.aux_streams.lock().await.as_ref().unwrap().len(), 1);
            let mut peer = BufReader::new(peer);
            assert!(
                timeout(Duration::from_millis(20), socket::read_json_line(&mut peer))
                    .await
                    .is_err()
            );
            client.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn disconnected_cached_input_rebinds_only_on_next_explicit_command() {
        use tokio::time::{timeout, Duration};

        timeout(Duration::from_secs(3), async {
            let execution = Execution {
                run_id: "pipeline".into(),
                stage_run_id: "exec-stage".into(),
                nonce: 9,
            };
            let (old_stream, old_peer) = UnixStream::pair().unwrap();
            let (new_stream, new_peer) = UnixStream::pair().unwrap();
            let (client, _peers) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                vec![old_stream, new_stream],
            );
            let client = Arc::new(client);
            let sending = Arc::clone(&client);
            let target = execution.clone();
            let first = tokio::spawn(async move {
                sending
                    .selected_input(&target, json!({"cmd":"pause", "req_id":"17"}))
                    .await
            });
            let mut old_peer = BufReader::new(old_peer);
            let bind = socket::read_json_line(&mut old_peer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bind, execution.input_request("owner", "17"));
            socket::write_json_line(
                old_peer.get_mut(),
                &json!({
                    "type":"input_bound", "id":bind["id"],
                    "stage_run_id":bind["stage_run_id"], "req_id":bind["req_id"]
                }),
            )
            .await
            .unwrap();
            assert_eq!(
                socket::read_json_line(&mut old_peer)
                    .await
                    .unwrap()
                    .unwrap(),
                json!({"cmd":"pause", "req_id":"17"})
            );
            let old_input = Arc::clone(
                &client
                    .selected_inputs
                    .lock()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()
                    .input,
            );
            drop(old_peer);
            assert!(first
                .await
                .unwrap()
                .unwrap_err()
                .starts_with("delivery uncertain:"));
            assert!(old_input.is_failed());
            assert_eq!(client.selected_inputs.lock().unwrap().len(), 1);
            assert_eq!(client.aux_streams.lock().await.as_ref().unwrap().len(), 1);
            let mut new_peer = BufReader::new(new_peer);
            assert!(timeout(
                Duration::from_millis(20),
                socket::read_json_line(&mut new_peer)
            )
            .await
            .is_err());

            let sending = Arc::clone(&client);
            let target = execution.clone();
            let second = tokio::spawn(async move {
                sending
                    .selected_input(&target, json!({"cmd":"resume", "req_id":"18"}))
                    .await
            });
            let bind = socket::read_json_line(&mut new_peer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bind, execution.input_request("owner", "18"));
            socket::write_json_line(
                new_peer.get_mut(),
                &json!({
                    "type":"input_bound", "id":bind["id"],
                    "stage_run_id":bind["stage_run_id"], "req_id":bind["req_id"]
                }),
            )
            .await
            .unwrap();
            assert_eq!(
                socket::read_json_line(&mut new_peer)
                    .await
                    .unwrap()
                    .unwrap(),
                json!({"cmd":"resume", "req_id":"18"})
            );
            {
                let inputs = client.selected_inputs.lock().unwrap();
                assert_eq!(inputs.len(), 1);
                let selected = inputs
                    .get(&(
                        execution.run_id.clone(),
                        execution.stage_run_id.clone(),
                        execution.nonce,
                    ))
                    .unwrap();
                assert!(!Arc::ptr_eq(&selected.input, &old_input));
            }
            socket::write_json_line(
                new_peer.get_mut(),
                &json!({"type":"delivered", "req_id":"18"}),
            )
            .await
            .unwrap();
            second.await.unwrap().unwrap();
            assert!(timeout(
                Duration::from_millis(20),
                socket::read_json_line(&mut new_peer)
            )
            .await
            .is_err());
            client.shutdown().await;
            assert!(socket::read_json_line(&mut new_peer)
                .await
                .unwrap()
                .is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn invalidate_and_shutdown_close_queued_input_with_live_waiter() {
        use tokio::time::{timeout, Duration};

        timeout(Duration::from_secs(3), async {
            for shutdown in [false, true] {
                let execution = Execution {
                    run_id: "pipeline".into(),
                    stage_run_id: "exec-stage".into(),
                    nonce: 9,
                };
                let (stream, peer) = UnixStream::pair().unwrap();
                let (client, _peers) = DaemonClient::fake_with_aux_streams(
                    json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                    vec![stream],
                );
                let client = Arc::new(client);
                let sending = Arc::clone(&client);
                let waiter = tokio::spawn(async move {
                    sending
                        .deliver_correction(&execution, 17, "queued correction")
                        .await
                });
                let mut peer = BufReader::new(peer);
                let bind = socket::read_json_line(&mut peer).await.unwrap().unwrap();
                socket::write_json_line(
                    peer.get_mut(),
                    &json!({
                        "type":"input_bound", "id":bind["id"],
                        "stage_run_id":bind["stage_run_id"], "req_id":bind["req_id"]
                    }),
                )
                .await
                .unwrap();
                assert_eq!(
                    socket::read_json_line(&mut peer).await.unwrap().unwrap(),
                    json!({"cmd":"message", "req_id":"17", "text":"queued correction"})
                );
                socket::write_json_line(peer.get_mut(), &json!({"type":"queued", "req_id":"17"}))
                    .await
                    .unwrap();
                let input = Arc::clone(
                    &client
                        .selected_inputs
                        .lock()
                        .unwrap()
                        .values()
                        .next()
                        .unwrap()
                        .input,
                );
                assert!(input.has_pending());
                assert!(Arc::strong_count(&input) >= 3);
                assert!(!waiter.is_finished());
                if shutdown {
                    client.shutdown().await;
                } else {
                    client.invalidate_inputs();
                }
                assert!(client.selected_inputs.lock().unwrap().is_empty());
                assert!(input.is_failed());
                assert!(!input.has_pending());
                assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
                assert_eq!(
                    waiter.await.unwrap().unwrap_err(),
                    "delivery uncertain: input client closed"
                );
                if !shutdown {
                    client.shutdown().await;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn invalidation_during_bind_rejects_command_without_writing() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let execution = Execution {
                run_id: "pipeline".into(),
                stage_run_id: "exec-stage".into(),
                nonce: 9,
            };
            let (stream, peer) = UnixStream::pair().unwrap();
            let (client, _peers) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                vec![stream],
            );
            let client = Arc::new(client);
            let sending = Arc::clone(&client);
            let task =
                tokio::spawn(
                    async move { sending.deliver_correction(&execution, 17, "stale").await },
                );
            let mut peer = BufReader::new(peer);
            let bind = socket::read_json_line(&mut peer).await.unwrap().unwrap();
            client.release_inputs();
            socket::write_json_line(
                peer.get_mut(),
                &json!({"type":"input_bound", "id":bind["id"],
                    "stage_run_id":bind["stage_run_id"], "req_id":bind["req_id"]}),
            )
            .await
            .unwrap();
            assert_eq!(
                task.await.unwrap().unwrap_err(),
                "input selection invalidated"
            );
            assert!(client.selected_inputs.lock().unwrap().is_empty());
            assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn release_preserves_pending_receipt_and_new_selection() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let execution = Execution {
                run_id: "pipeline".into(),
                stage_run_id: "exec-stage".into(),
                nonce: 9,
            };
            let (old_stream, old_peer) = UnixStream::pair().unwrap();
            let (new_stream, new_peer) = UnixStream::pair().unwrap();
            let (client, _peers) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                vec![old_stream, new_stream],
            );
            let client = Arc::new(client);
            let mut peers = [BufReader::new(old_peer), BufReader::new(new_peer)];
            let mut tasks = Vec::new();
            for (index, peer) in peers.iter_mut().enumerate() {
                let sending = Arc::clone(&client);
                let mut execution = execution.clone();
                execution.stage_run_id = format!("exec-{index}");
                tasks.push(tokio::spawn(async move {
                    sending
                        .deliver_correction(&execution, index as u64, "text")
                        .await
                }));
                let bind = socket::read_json_line(peer).await.unwrap().unwrap();
                socket::write_json_line(
                    peer.get_mut(),
                    &json!({"type":"input_bound", "id":bind["id"],
                        "stage_run_id":bind["stage_run_id"], "req_id":bind["req_id"]}),
                )
                .await
                .unwrap();
                let command = socket::read_json_line(peer).await.unwrap().unwrap();
                assert_eq!(command["req_id"], index.to_string());
                if index == 0 {
                    socket::write_json_line(
                        peer.get_mut(),
                        &json!({"type":"queued", "req_id":"0"}),
                    )
                    .await
                    .unwrap();
                    client.release_inputs();
                }
            }
            socket::write_json_line(
                peers[0].get_mut(),
                &json!({"type":"delivered", "req_id":"0"}),
            )
            .await
            .unwrap();
            tasks.remove(0).await.unwrap().unwrap();
            // A settles in the background while B remains selected and pending.
            assert!(!tasks[0].is_finished());
            assert!(socket::read_json_line(&mut peers[0])
                .await
                .unwrap()
                .is_none());
            {
                let inputs = client.selected_inputs.lock().unwrap();
                assert_eq!(inputs.len(), 1);
                assert!(inputs.keys().any(|key| key.1 == "exec-1"));
            }
            socket::write_json_line(
                peers[1].get_mut(),
                &json!({"type":"delivered", "req_id":"1"}),
            )
            .await
            .unwrap();
            tasks.remove(0).await.unwrap().unwrap();
            assert_eq!(client.selected_inputs.lock().unwrap().len(), 1);
            client.release_inputs();
            assert!(client.selected_inputs.lock().unwrap().is_empty());
            assert!(socket::read_json_line(&mut peers[1])
                .await
                .unwrap()
                .is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn deselected_queued_correction_resumes_on_original_binding() {
        use tokio::time::{timeout, Duration};

        timeout(Duration::from_secs(3), async {
            let execution = Execution {
                run_id: "pipeline".into(),
                stage_run_id: "exec-stage".into(),
                nonce: 9,
            };
            let (stream, peer) = UnixStream::pair().unwrap();
            let (client, peers) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                vec![stream],
            );
            let client = Arc::new(client);
            let sending = Arc::clone(&client);
            let target = execution.clone();
            let correction =
                tokio::spawn(
                    async move { sending.deliver_correction(&target, 17, "correction").await },
                );
            let mut peer = BufReader::new(peer);
            let bind = socket::read_json_line(&mut peer).await.unwrap().unwrap();
            socket::write_json_line(
                peer.get_mut(),
                &json!({"type":"input_bound", "id":bind["id"],
                    "stage_run_id":bind["stage_run_id"], "req_id":bind["req_id"]}),
            )
            .await
            .unwrap();
            assert_eq!(
                socket::read_json_line(&mut peer).await.unwrap().unwrap(),
                json!({"cmd":"message", "req_id":"17", "text":"correction"})
            );
            socket::write_json_line(peer.get_mut(), &json!({"type":"queued", "req_id":"17"}))
                .await
                .unwrap();

            // Leaving and returning must neither control execution nor lose its binding.
            client.release_inputs();
            client.release_inputs();
            assert!(!correction.is_finished());
            assert!(
                timeout(Duration::from_millis(20), socket::read_json_line(&mut peer))
                    .await
                    .is_err()
            );
            for stream in peers {
                let mut stream = BufReader::new(stream);
                assert!(timeout(
                    Duration::from_millis(20),
                    socket::read_json_line(&mut stream)
                )
                .await
                .is_err());
            }
            let sending = Arc::clone(&client);
            let resume = tokio::spawn(async move {
                sending
                    .selected_input(&execution, json!({"cmd":"resume", "req_id":"18"}))
                    .await
            });
            assert_eq!(
                socket::read_json_line(&mut peer).await.unwrap().unwrap(),
                json!({"cmd":"resume", "req_id":"18"})
            );
            socket::write_json_line(peer.get_mut(), &json!({"type":"delivered", "req_id":"18"}))
                .await
                .unwrap();
            resume.await.unwrap().unwrap();
            assert!(!correction.is_finished());
            socket::write_json_line(peer.get_mut(), &json!({"type":"delivered", "req_id":"17"}))
                .await
                .unwrap();
            correction.await.unwrap().unwrap();
            // Reopening A claims the binding for the current selection; the old
            // message's completion must not release it.
            assert_eq!(client.selected_inputs.lock().unwrap().len(), 1);
            client.release_inputs();
            assert!(client.selected_inputs.lock().unwrap().is_empty());
            assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn correction_and_controls_reuse_one_native_input_socket() {
        use tokio::time::{timeout, Duration};

        timeout(Duration::from_secs(3), async {
            let execution = Execution {
                run_id: "pipeline".into(),
                stage_run_id: "exec-stage".into(),
                nonce: 9,
            };
            let (stream, peer) = UnixStream::pair().unwrap();
            let (client, [_control, _session]) = DaemonClient::fake_with_aux_streams(
                json!({"session_id":"owner", "chat_run_id":"session_chat"}),
                vec![stream],
            );
            let server = tokio::spawn(async move {
                let mut peer = BufReader::new(peer);
                let mut request = socket::read_json_line(&mut peer).await.unwrap().unwrap();
                // Only the native binding fields and an optional nonce are allowed.
                if let Some(nonce) = request.as_object_mut().unwrap().remove("nonce") {
                    assert_eq!(nonce, json!(9));
                }
                assert_eq!(
                    request,
                    json!({"op":"input", "session_id":"owner", "id":"pipeline",
                        "stage_run_id":"exec-stage", "req_id":"17"})
                );
                socket::write_json_line(
                    peer.get_mut(),
                    &json!({
                        "type":"input_bound", "id":"pipeline",
                        "stage_run_id":"exec-stage", "req_id":"17"
                    }),
                )
                .await
                .unwrap();
                for (cmd, req_id) in [
                    ("message", "17"),
                    ("pause", "18"),
                    ("step", "19"),
                    ("resume", "20"),
                    ("bail", "21"),
                ] {
                    let command = socket::read_json_line(&mut peer).await.unwrap().unwrap();
                    let mut expected = json!({"cmd":cmd, "req_id":req_id});
                    if cmd == "message" {
                        expected["text"] = json!("  stop here\n");
                    }
                    assert_eq!(command, expected);
                    socket::write_json_line(
                        peer.get_mut(),
                        &json!({"type":"delivered", "req_id":req_id}),
                    )
                    .await
                    .unwrap();
                }
                assert!(socket::read_json_line(&mut peer).await.unwrap().is_none());
            });

            client
                .deliver_correction(&execution, 17, "  stop here\n")
                .await
                .unwrap();
            for (cmd, req_id) in [
                ("pause", "18"),
                ("step", "19"),
                ("resume", "20"),
                ("bail", "21"),
            ] {
                client
                    .selected_input(&execution, json!({"cmd":cmd, "req_id":req_id}))
                    .await
                    .unwrap();
            }
            client.shutdown().await;
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}
