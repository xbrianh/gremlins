//! Persistent socket client for the TUI.
//!
//! Owns a long-lived Unix-domain socket connection to the executor daemon.
//! Provides request/response helpers and a stream of [`DaemonEvent`] values
//! received as unsolicited broadcasts from the supervisor.
//!
//! Log following uses a separate short-lived connection because the daemon's
//! `log` op (with `follow: true`) is a streaming op that monopolises its
//! connection. Keeping it on its own socket lets the main connection stay
//! open for other commands.

use std::collections::VecDeque;
use std::sync::Arc;

use gremlins::executor::socket;
use gremlins::executor::DaemonEvent;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex};

/// A persistent connection to the executor daemon.
///
/// The read half is driven by a background task that routes every incoming
/// JSON-line to either the oldest pending request waiter (oneshot) or the
/// daemon-event broadcast channel.
pub struct DaemonClient {
    write_half: Mutex<OwnedWriteHalf>,
    state: Arc<ReadState>,
    /// Handle for the background read-loop task. Aborted on shutdown to
    /// close the socket read half.
    read_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct ReadState {
    /// Pending response waiters in request order. The daemon processes
    /// requests sequentially on a connection and writes each response in
    /// order, so FIFO delivery is correct.
    response_txs: Mutex<VecDeque<oneshot::Sender<Value>>>,
    /// Daemon events forwarded to the TUI event loop.
    event_tx: mpsc::UnboundedSender<DaemonEvent>,
    /// Raw JSON lines that are neither responses nor DaemonEvents.
    raw_tx: mpsc::UnboundedSender<Value>,
}

/// Connect to the executor daemon and return the client handle plus
/// receivers for unsolicited [`DaemonEvent`] values and raw lines.
pub async fn connect() -> Result<
    (
        DaemonClient,
        mpsc::UnboundedReceiver<DaemonEvent>,
        mpsc::UnboundedReceiver<Value>,
    ),
    String,
> {
    let stream = crate::spawn::ensure_executor().await?;
    let (read_half, write_half) = stream.into_split();

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel();
    let state = Arc::new(ReadState {
        response_txs: Mutex::new(VecDeque::new()),
        event_tx,
        raw_tx,
    });

    let read_state = Arc::clone(&state);
    let read_handle = tokio::spawn(async move {
        read_loop(read_half, read_state).await;
    });

    Ok((
        DaemonClient {
            write_half: Mutex::new(write_half),
            state,
            read_handle: Mutex::new(Some(read_handle)),
        },
        event_rx,
        raw_rx,
    ))
}

impl DaemonClient {
    /// Send a JSON request and wait for the response.
    ///
    /// Serialises waiter registration and the socket write under the same
    /// lock so that request order on the wire matches response order.
    pub async fn send_request(&self, request: Value) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();

        // Hold the write lock across enqueue + write so concurrent callers
        // cannot interleave.
        let mut write_half = self.write_half.lock().await;
        {
            let mut guard = self.state.response_txs.lock().await;
            guard.push_back(tx);
        }

        if let Err(e) = socket::write_json_line(&mut *write_half, &request).await {
            // Write failed — remove our waiter so the queue stays aligned.
            let mut guard = self.state.response_txs.lock().await;
            guard.pop_back();
            return Err(e);
        }
        drop(write_half);

        rx.await.map_err(|_| "connection closed".to_string())
    }

    /// Abort the background read loop, closing the socket read half.
    pub async fn shutdown(&self) {
        if let Some(handle) = self.read_handle.lock().await.take() {
            handle.abort();
        }
    }
}

/// Open a dedicated connection for a streaming log follow.
///
/// Returns a receiver of raw JSON lines (each `{"type": "log_line", ...}`)
/// and the task handle driving the read loop. Dropping/aborting the task
/// closes the follow connection without disturbing the main client socket.
pub async fn follow_log(
    id: &str,
) -> Result<(mpsc::UnboundedReceiver<Value>, tokio::task::JoinHandle<()>), String> {
    let state_root = gremlins::config::state_root();
    let mut stream = socket::connect_socket(&state_root).await?;

    let request = serde_json::json!({"op": "log", "id": id, "follow": true});
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

/// Background reader: deserialise every incoming line.
///
/// Unsolicited [`DaemonEvent`] values are classified first and forwarded
/// to the TUI event loop. Everything else is treated as a response to the
/// oldest pending request (FIFO). If no request is waiting, the line is
/// forwarded as a raw JSON value.
async fn read_loop(read_half: OwnedReadHalf, state: Arc<ReadState>) {
    let mut reader = BufReader::new(read_half);
    loop {
        let value = match socket::read_json_line(&mut reader).await {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => break,
        };

        // Classify as a DaemonEvent first — unsolicited events can arrive
        // at any time, even when a request is pending.
        if let Ok(event) = serde_json::from_value::<DaemonEvent>(value.clone()) {
            let _ = state.event_tx.send(event);
            continue;
        }

        // Not an event — deliver as a response to the oldest pending request.
        let response_tx = {
            let mut guard = state.response_txs.lock().await;
            guard.pop_front()
        };

        if let Some(tx) = response_tx {
            let _ = tx.send(value);
            continue;
        }

        // Forward as a raw line (log_line, status, etc.).
        let _ = state.raw_tx.send(value);
    }
}
