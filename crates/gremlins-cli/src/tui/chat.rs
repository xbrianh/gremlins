//! Chat client that sends per-message requests to the daemon.

use gremlins::executor::socket;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum ChatEvent {
    StreamChunk(String),
    ReasoningChunk(String),
    ToolResult {
        name: String,
    },
    TurnComplete {
        text: String,
        tool_calls: Vec<String>,
    },
    Done {
        text: String,
        usage: Option<Value>,
        /// The gremlin id, carried so the TUI can capture it on the first
        /// turn and reuse it for subsequent messages.
        id: Option<String>,
    },
    Ended {
        reason: String,
        /// The gremlin id, carried on terminal payloads so the TUI can
        /// capture it even when the first turn ends abnormally.
        id: Option<String>,
    },
    Error {
        message: String,
        /// The gremlin id, carried on terminal payloads so the TUI can
        /// capture it even when the first turn fails.
        id: Option<String>,
    },
}

/// Send a chat message and receive stream events until Done.
///
/// Uses a fresh connection per message. The daemon owns the conversation
/// history; the client sends only the gremlin id (absent on first contact)
/// and the user text.
///
/// `cancel_rx` is a oneshot that the caller fires to cancel the
/// socket-reader task. When fired, the reader drops `write_half`
/// so the daemon sees EOF and stops the agent loop immediately.
pub async fn send_message(
    chat_id: Option<&str>,
    text: &str,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<mpsc::UnboundedReceiver<ChatEvent>, String> {
    let state_root = gremlins::config::state_root();
    let stream = socket::connect_socket(&state_root).await?;
    let (read_half, mut write_half) = stream.into_split();

    // Send the chat request — only the gremlin id and text.
    let mut request = serde_json::json!({
        "op": "chat",
        "text": text,
    });
    if let Some(id) = chat_id {
        request["id"] = serde_json::Value::String(id.to_string());
    }
    socket::write_json_line(&mut write_half, &request).await?;

    let (event_tx, event_rx) = mpsc::unbounded_channel::<ChatEvent>();

    // Keep write_half alive in the spawned task so the daemon does not
    // see EOF and bail out of the stream-forwarding loop before
    // producing any events.
    //
    // The reader monitors cancel_rx so the caller (Esc) can force-close
    // the socket by dropping write_half, stopping the agent immediately.
    tokio::spawn(async move {
        tokio::pin!(cancel_rx);
        let _write_half = write_half;
        let mut reader = BufReader::new(read_half);
        loop {
            tokio::select! {
                result = socket::read_json_line(&mut reader) => {
                    match result {
                        Ok(Some(value)) => {
                            if is_daemon_broadcast(&value) {
                                continue;
                            }
                            let event = parse_chat_event(&value);
                            let is_terminal = matches!(
                                &event,
                                ChatEvent::Done { .. } | ChatEvent::Ended { .. } | ChatEvent::Error { .. }
                            );
                            let _ = event_tx.send(event);
                            if is_terminal {
                                break;
                            }
                        }
                        Ok(None) | Err(_) => {
                            let _ = event_tx.send(ChatEvent::Ended {
                                reason: "disconnect".to_string(),
                                id: None,
                            });
                            break;
                        }
                    }
                }
                _ = &mut cancel_rx => {
                    // Esc pressed — drop write_half to close the socket
                    // so the daemon sees EOF and stops the agent loop.
                    break;
                }
            }
        }
    });

    Ok(event_rx)
}

fn parse_chat_event(value: &Value) -> ChatEvent {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("stream_chunk") => ChatEvent::StreamChunk(
            value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        Some("reasoning_chunk") => ChatEvent::ReasoningChunk(
            value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        Some("tool_result") => ChatEvent::ToolResult {
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("turn_complete") => ChatEvent::TurnComplete {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tool_calls: value
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        },
        Some("done") => ChatEvent::Done {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            usage: value.get("usage").cloned(),
            id: value.get("id").and_then(|v| v.as_str()).map(String::from),
        },
        Some("ended") => ChatEvent::Ended {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            id: value.get("id").and_then(|v| v.as_str()).map(String::from),
        },
        Some("error") => ChatEvent::Error {
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string(),
            id: value.get("id").and_then(|v| v.as_str()).map(String::from),
        },
        _ => ChatEvent::Error {
            message: format!("unexpected event: {value}"),
            id: None,
        },
    }
}

fn is_daemon_broadcast(value: &Value) -> bool {
    match value.get("type").and_then(|v| v.as_str()) {
        Some(t) if t.starts_with("run_") => true,
        Some("event") | Some("stage_transition") | Some("log_line") | Some("bail") => true,
        _ => false,
    }
}
