//! Thin chat client that speaks the debug protocol over a dedicated socket.

use gremlins::executor::socket;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Ready,
    TurnComplete {
        #[allow(dead_code)]
        turn: usize,
        text: String,
        tool_calls: Vec<String>,
    },
    Done {
        text: String,
        #[allow(dead_code)]
        usage: Option<Value>,
    },
    Ended {
        reason: String,
    },
    Error(String),
}

pub struct ChatGremlin {
    cmd_tx: mpsc::UnboundedSender<Value>,
    _read_task: AbortOnDrop,
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ChatGremlin {
    pub async fn start() -> Result<(Self, mpsc::UnboundedReceiver<ChatEvent>), String> {
        let state_root = gremlins::config::state_root();
        let stream = socket::connect_socket(&state_root).await?;
        let (read_half, mut write_half) = stream.into_split();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ChatEvent>();

        // Send the chat op to build and attach.
        socket::write_json_line(&mut write_half, &serde_json::json!({"op": "chat"})).await?;

        // Await the daemon's confirmation before declaring ready.
        // The daemon may send debug_status messages before debug_ready.
        let mut reader = BufReader::new(read_half);
        loop {
            match socket::read_json_line(&mut reader).await {
                Ok(Some(value)) => match value.get("type").and_then(|v| v.as_str()) {
                    Some("debug_ready") => break,
                    Some("debug_status") => continue,
                    Some("error") => {
                        let msg = value
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        return Err(msg.to_string());
                    }
                    // Daemon broadcast events arrive on this
                    // connection; silently skip them.
                    _ if is_daemon_broadcast(&value) => {
                        continue;
                    }
                    _ => {
                        return Err(format!("unexpected daemon response: {value}"));
                    }
                },
                Ok(None) | Err(_) => {
                    return Err("daemon disconnected".to_string());
                }
            }
        }
        let read_half = reader.into_inner();

        let read_task = tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            loop {
                tokio::select! {
                    result = socket::read_json_line(&mut reader) => {
                        match result {
                            Ok(Some(value)) => {
                                // Daemon broadcast events arrive on this
                                // connection; silently skip them.
                                if is_daemon_broadcast(&value) {
                                    continue;
                                }
                                let event = parse_chat_event(&value);
                                if event_tx.send(event).is_err() {
                                    break;
                                }
                            }
                            Ok(None) | Err(_) => {
                                let _ = event_tx.send(ChatEvent::Ended {
                                    reason: "disconnect".to_string(),
                                });
                                break;
                            }
                        }
                    }
                    Some(cmd) = cmd_rx.recv() => {
                        if socket::write_json_line(&mut write_half, &cmd).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                cmd_tx,
                _read_task: AbortOnDrop(read_task),
            },
            event_rx,
        ))
    }

    pub fn talk(&self, text: &str) {
        let _ = self
            .cmd_tx
            .send(serde_json::json!({"op": "talk", "text": text}));
    }

    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(serde_json::json!({"op": "quit"}));
    }

    pub fn continue_turn(&self) {
        let _ = self.cmd_tx.send(serde_json::json!({"op": "continue"}));
    }
}

fn parse_chat_event(value: &Value) -> ChatEvent {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("debug_ready") => ChatEvent::Ready,
        Some("debug_turn_complete") => ChatEvent::TurnComplete {
            turn: value.get("turn").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
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
        Some("debug_done") => ChatEvent::Done {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            usage: value.get("usage").cloned(),
        },
        Some("debug_ended") => ChatEvent::Ended {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("debug_status") => {
            // Status updates from the daemon during startup; silently ignore.
            ChatEvent::Ready
        }
        Some("error") => ChatEvent::Error(
            value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string(),
        ),
        _ => ChatEvent::Error(format!("unexpected event: {value}")),
    }
}

/// Returns true if `value` is a daemon broadcast event that the chat
/// client should silently discard.
fn is_daemon_broadcast(value: &Value) -> bool {
    match value.get("type").and_then(|v| v.as_str()) {
        Some(t) if t.starts_with("run_") => true,
        Some("event") | Some("stage_transition") | Some("log_line") | Some("bail") => true,
        _ => false,
    }
}
