//! Chat event types shared with the TUI event loop.
//!
//! Events arrive as unsolicited JSON lines on the single persistent
//! daemon connection and are classified by `tui::client::read_loop`.

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum ChatEvent {
    StreamChunk {
        text: String,
        turn: Option<String>,
    },
    ReasoningChunk {
        text: String,
        turn: Option<String>,
    },
    ToolResult {
        name: String,
        output: String,
        turn: Option<String>,
    },
    TurnComplete {
        text: String,
        tool_calls: Vec<String>,
        turn: Option<String>,
    },
    Done {
        text: String,
        /// The `Done` tool's `summary` argument, appended to the turn's
        /// model text at turn end.
        summary: String,
        usage: Option<Value>,
        /// The gremlin id, carried so the TUI can capture it on the first
        /// turn and reuse it for subsequent messages.
        id: Option<String>,
        turn: Option<String>,
    },
    Ended {
        reason: String,
        /// The gremlin id, carried on terminal payloads so the TUI can
        /// capture it even when the first turn ends abnormally.
        id: Option<String>,
        turn: Option<String>,
    },
    Error {
        message: String,
        /// The gremlin id, carried on terminal payloads so the TUI can
        /// capture it even when the first turn fails.
        id: Option<String>,
        turn: Option<String>,
    },
    /// The daemon's ack for a chat request carries the gremlin id.
    /// The TUI stores it so subsequent messages target the same gremlin,
    /// even if Esc is pressed before Done/Ended arrives — without this
    /// the next submit would create a second chat gremlin.
    ChatId {
        id: String,
        turn: Option<String>,
    },
}

/// Parse a daemon chat-protocol JSON line into a [`ChatEvent`].
pub(crate) fn parse_chat_event(value: &Value) -> Option<ChatEvent> {
    let turn = value.get("turn").and_then(|v| v.as_str()).map(String::from);
    match value.get("type").and_then(|v| v.as_str()) {
        Some("stream_chunk") => Some(ChatEvent::StreamChunk {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turn,
        }),
        Some("reasoning_chunk") => Some(ChatEvent::ReasoningChunk {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turn,
        }),
        Some("tool_result") => Some(ChatEvent::ToolResult {
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
            turn,
        }),
        Some("turn_complete") => Some(ChatEvent::TurnComplete {
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
            turn,
        }),
        Some("done") => Some(ChatEvent::Done {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            summary: value
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            usage: value.get("usage").cloned(),
            id: value.get("id").and_then(|v| v.as_str()).map(String::from),
            turn,
        }),
        Some("ended") => Some(ChatEvent::Ended {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            id: value.get("id").and_then(|v| v.as_str()).map(String::from),
            turn,
        }),
        Some("error") => {
            // Chat-terminal errors carry a gremlin id. Bare `type: "error"`
            // lines are request/response errors and must stay in the FIFO
            // response queue, so only classify id-bearing errors as chat.
            let id = value.get("id").and_then(|v| v.as_str()).map(String::from);
            id.map(|id| ChatEvent::Error {
                message: value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
                id: Some(id),
                turn,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_done_carries_summary() {
        let value = serde_json::json!({
            "type": "done",
            "id": "chat-0000",
            "text": "streamed response",
            "summary": "answered the question",
            "usage": {"input_tokens": 10, "output_tokens": 20},
            "turn": "3",
        });
        match parse_chat_event(&value) {
            Some(ChatEvent::Done {
                text,
                summary,
                id,
                turn,
                ..
            }) => {
                assert_eq!(text, "streamed response");
                assert_eq!(summary, "answered the question");
                assert_eq!(id.as_deref(), Some("chat-0000"));
                assert_eq!(turn.as_deref(), Some("3"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn parse_done_missing_summary_defaults_to_empty() {
        let value = serde_json::json!({"type": "done", "text": "hi"});
        match parse_chat_event(&value) {
            Some(ChatEvent::Done { summary, .. }) => {
                assert_eq!(summary, "");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
