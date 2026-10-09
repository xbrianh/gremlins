/// Result of executing a slash command.
pub enum CommandResult {
    /// Lines to append to the output buffer.
    Lines(Vec<String>),
    /// Clear output + input and restart the chat agent (a fresh session).
    RestartChat,
    /// Exit the TUI.
    Quit,
    /// Send a JSON op over the socket (handled by the event loop).
    SocketOp {
        op: String,
        payload: serde_json::Value,
    },
    /// Show conversation history (handled by event loop with App state).
    ShowHistory,
    /// Truncate conversation history at the given 0-based index.
    TruncateHistory(usize),
}

/// Parse `input` (minus the leading `/`) and dispatch.
///
/// Returns `None` when the input is not a slash command (plain text).
pub fn dispatch(input: &str) -> Option<CommandResult> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let body = &trimmed[1..]; // strip the leading '/'
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();

    match cmd {
        "chat" => Some(CommandResult::Lines(vec![
            "chat is always active — type your message directly".to_string(),
        ])),
        "clear" => Some(CommandResult::RestartChat),
        "new" => Some(CommandResult::RestartChat),
        "quit" => Some(CommandResult::Quit),
        "help" => Some(CommandResult::Lines(help_text())),
        "ls" => Some(CommandResult::SocketOp {
            op: "ls".to_string(),
            payload: serde_json::json!({"op": "ls"}),
        }),
        "info" => {
            let id = args.first().copied().unwrap_or("");
            if id.is_empty() {
                return Some(CommandResult::Lines(vec![
                    "> /info".to_string(),
                    "usage: /info <id>".to_string(),
                ]));
            }
            Some(CommandResult::SocketOp {
                op: "info".to_string(),
                payload: serde_json::json!({"op": "info", "id": id}),
            })
        }
        "stop" => {
            let id = args.first().copied().unwrap_or("");
            if id.is_empty() {
                return Some(CommandResult::Lines(vec![
                    "> /stop".to_string(),
                    "usage: /stop <id>".to_string(),
                ]));
            }
            Some(CommandResult::SocketOp {
                op: "stop".to_string(),
                payload: serde_json::json!({"op": "stop", "id": id}),
            })
        }
        "resume" => {
            let id = args.first().copied().unwrap_or("");
            if id.is_empty() {
                return Some(CommandResult::Lines(vec![
                    "> /resume".to_string(),
                    "usage: /resume <id>".to_string(),
                ]));
            }
            Some(CommandResult::SocketOp {
                op: "resume".to_string(),
                payload: serde_json::json!({"op": "resume", "id": id}),
            })
        }
        "log" => {
            let id = args.first().copied().unwrap_or("");
            if id.is_empty() {
                return Some(CommandResult::Lines(vec![
                    "> /log".to_string(),
                    "usage: /log <id>".to_string(),
                ]));
            }
            Some(CommandResult::SocketOp {
                op: "log".to_string(),
                payload: serde_json::json!({"op": "log", "id": id, "follow": true}),
            })
        }
        "model" => Some(CommandResult::Lines(vec![format!(
            "default client: {}",
            gremlins::config::global_config()
                .ok()
                .and_then(|c| c.default_client().map(String::from))
                .unwrap_or_else(|| "(not configured)".to_string())
        )])),
        "history" => Some(CommandResult::ShowHistory),
        "rollback" => {
            let idx: usize = match args.first().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None => {
                    return Some(CommandResult::Lines(vec![
                        "usage: /rollback <n> — truncate history at turn n".to_string(),
                    ]));
                }
            };
            Some(CommandResult::TruncateHistory(idx))
        }
        _ => Some(CommandResult::Lines(vec![format!(
            "unknown command: /{cmd} — type /help for available commands"
        )])),
    }
}

fn help_text() -> Vec<String> {
    vec![
        "available commands:".to_string(),
        "  /ls              — list gremlins".to_string(),
        "  /info <id>       — show gremlin info".to_string(),
        "  /stop <id>       — stop a gremlin".to_string(),
        "  /resume <id>     — resume a gremlin".to_string(),
        "  /log <id>        — show gremlin log".to_string(),
        "  /model           — show default client".to_string(),
        "  /history         — show conversation history".to_string(),
        "  /rollback <n>    — truncate history at turn n".to_string(),
        "  /chat            — chat is always active (type directly)".to_string(),
        "  /clear, /new     — restart chat".to_string(),
        "  /help            — show this help".to_string(),
        "  /quit            — exit".to_string(),
        "".to_string(),
        "keybindings:".to_string(),
        "  Ctrl+C           — exit".to_string(),
        "  Ctrl+D (empty)   — exit".to_string(),
        "  Ctrl+L           — redraw screen".to_string(),
        "  Esc              — clear input".to_string(),
        "  Ctrl+G           — editor (stub)".to_string(),
        "".to_string(),
        "scrolling: use terminal scrollback / tmux copy mode".to_string(),
    ]
}
