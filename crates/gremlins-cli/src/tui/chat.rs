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
        output: String,
    },
    TurnComplete {
        #[allow(dead_code)]
        turn: usize,
        text: String,
        #[allow(dead_code)]
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

/// Send a chat message and receive stream events until Done.
///
/// Uses a fresh connection per message. The daemon processes each
/// chat request as an ephemeral stage that runs to completion.
pub async fn send_message(
    text: &str,
    history: &[Value],
) -> Result<mpsc::UnboundedReceiver<ChatEvent>, String> {
    let state_root = gremlins::config::state_root();
    let stream = socket::connect_socket(&state_root).await?;
    let (read_half, mut write_half) = stream.into_split();

    // Send the chat request
    let request = serde_json::json!({
        "op": "chat",
        "text": text,
        "history": history,
    });
    socket::write_json_line(&mut write_half, &request).await?;

    let (event_tx, event_rx) = mpsc::unbounded_channel::<ChatEvent>();

    // Keep write_half alive in the spawned task so the daemon does not
    // see EOF and bail out of the stream-forwarding loop before
    // producing any events.
    tokio::spawn(async move {
        let _write_half = write_half;
        let mut reader = BufReader::new(read_half);
        loop {
            match socket::read_json_line(&mut reader).await {
                Ok(Some(value)) => {
                    if is_daemon_broadcast(&value) {
                        continue;
                    }
                    let event = parse_chat_event(&value);
                    let is_terminal = matches!(
                        &event,
                        ChatEvent::Done { .. } | ChatEvent::Ended { .. } | ChatEvent::Error(_)
                    );
                    let _ = event_tx.send(event);
                    if is_terminal {
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
            output: value
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("turn_complete") => ChatEvent::TurnComplete {
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
        Some("done") => ChatEvent::Done {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            usage: value.get("usage").cloned(),
        },
        Some("ended") => ChatEvent::Ended {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
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

fn is_daemon_broadcast(value: &Value) -> bool {
    match value.get("type").and_then(|v| v.as_str()) {
        Some(t) if t.starts_with("run_") => true,
        Some("event") | Some("stage_transition") | Some("log_line") | Some("bail") => true,
        _ => false,
    }
}
