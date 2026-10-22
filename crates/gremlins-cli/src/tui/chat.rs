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
    ToolOutputChunk {
        id: String,
        name: String,
        /// Formatted tool call (`format_tool_call` output) used as the
        /// live tool-slot title row.
        call: String,
        text: String,
        turn: Option<String>,
    },
    ToolResult {
        id: String,
        name: String,
        /// Formatted tool call (`format_tool_call` output) used as the
        /// committed tool-block title row.
        call: String,
        output: String,
        turn: Option<String>,
    },
    TurnComplete {
        text: String,
        tool_calls: Vec<String>,
        /// Per-turn token/cache delta from the completed agent-loop turn.
        stats: Option<Value>,
        /// Conversation-wide token/cache total (the current prompt's running
        /// total plus the persisted chat-gremlin baseline).
        total: Option<Value>,
        /// Chat request ID for stale-event filtering. Not the agent-loop
        /// iteration — use [`loop_turn`] for the loop counter.
        turn: Option<String>,
        /// Agent-loop iteration counter (1-based within the current
        /// prompt). Used to render the turn-number prefix in the
        /// reasoning-stream stats title.
        loop_turn: Option<usize>,
    },
    Done {
        text: String,
        /// The `Done` tool's `summary` argument, appended to the turn's
        /// model text at turn end.
        summary: String,
        /// Conversation-wide token/cache delta carried by the daemon's
        /// `done` event. The TUI no longer renders this line directly; the
        /// `TurnComplete` stats title already reports the same figures.
        #[allow(dead_code)]
        usage: Option<Value>,
        turn: Option<String>,
    },
    Ended {
        reason: String,
        turn: Option<String>,
    },
    Error {
        message: String,
        turn: Option<String>,
    },
    /// Non-fatal warning surfaced to the chat transcript (e.g. unresolved
    /// skill names). The turn continues normally after display.
    SystemWarning {
        message: String,
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
        Some("tool_output_chunk") => Some(ChatEvent::ToolOutputChunk {
            id: value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            call: value
                .get("call")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turn,
        }),
        Some("tool_result") => Some(ChatEvent::ToolResult {
            id: value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            call: value
                .get("call")
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
            stats: value.get("stats").filter(|v| !v.is_null()).cloned(),
            total: value.get("total").filter(|v| !v.is_null()).cloned(),
            turn,
            loop_turn: value
                .get("loop_turn")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize),
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
            turn,
        }),
        Some("ended") => Some(ChatEvent::Ended {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turn,
        }),
        Some("error") => {
            // Chat-terminal errors carry a gremlin id. Bare `type: "error"`
            // lines are request/response errors and must stay in the FIFO
            // response queue, so only classify id-bearing errors as chat.
            let id = value.get("id").and_then(|v| v.as_str()).map(String::from);
            id.map(|_| ChatEvent::Error {
                message: value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
                turn,
            })
        }
        Some("system_warning") => Some(ChatEvent::SystemWarning {
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turn,
        }),
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
                turn,
                ..
            }) => {
                assert_eq!(text, "streamed response");
                assert_eq!(summary, "answered the question");
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

    #[test]
    fn parse_tool_output_chunk_carries_id_name_call_and_turn() {
        let value = serde_json::json!({
            "type": "tool_output_chunk",
            "id": "call_9",
            "name": "Bash",
            "call": "Bash(command=\"make -j8 check && make -j8 test\")",
            "text": "hello\n",
            "turn": "7",
        });
        match parse_chat_event(&value) {
            Some(ChatEvent::ToolOutputChunk {
                id,
                name,
                call,
                text,
                turn,
            }) => {
                assert_eq!(id, "call_9");
                assert_eq!(name, "Bash");
                assert_eq!(call, "Bash(command=\"make -j8 check && make -j8 test\")");
                assert_eq!(text, "hello\n");
                assert_eq!(turn.as_deref(), Some("7"));
            }
            other => panic!("expected ToolOutputChunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_result_carries_id_and_call() {
        let value = serde_json::json!({
            "type": "tool_result",
            "id": "call_11",
            "name": "Grep",
            "call": "Grep(pattern=\"fn main\")",
            "output": "src/main.rs:1:fn main() {}",
            "turn": "8",
        });
        match parse_chat_event(&value) {
            Some(ChatEvent::ToolResult {
                id,
                name,
                call,
                output,
                turn,
            }) => {
                assert_eq!(id, "call_11");
                assert_eq!(name, "Grep");
                assert_eq!(call, "Grep(pattern=\"fn main\")");
                assert_eq!(output, "src/main.rs:1:fn main() {}");
                assert_eq!(turn.as_deref(), Some("8"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parse_turn_complete_distinguishes_request_id_from_loop_turn() {
        // The `turn` field is the chat request ID (for stale-event
        // filtering). It stays constant across iterations of a single
        // prompt. The `loop_turn` field is the agent-loop iteration
        // counter and increments on every agent iteration.
        let value = serde_json::json!({
            "type": "turn_complete",
            "turn": "5",
            "loop_turn": 3,
            "text": "iteration three",
            "tool_calls": [],
        });
        match parse_chat_event(&value) {
            Some(ChatEvent::TurnComplete {
                turn,
                loop_turn,
                text,
                ..
            }) => {
                assert_eq!(turn.as_deref(), Some("5"), "request ID");
                assert_eq!(loop_turn, Some(3), "loop iteration");
                assert_eq!(text, "iteration three");
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }

    #[test]
    fn parse_turn_complete_missing_loop_turn_is_none() {
        // When `loop_turn` is absent (e.g. from an older daemon), the
        // field is `None` and the consumer falls back to 0.
        let value = serde_json::json!({
            "type": "turn_complete",
            "turn": "1",
            "text": "ok",
            "tool_calls": [],
        });
        match parse_chat_event(&value) {
            Some(ChatEvent::TurnComplete { loop_turn, .. }) => {
                assert_eq!(loop_turn, None);
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }
}
