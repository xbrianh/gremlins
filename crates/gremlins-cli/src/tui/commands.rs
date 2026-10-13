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
    /// Toggle the non-blocking gremlins watch table.
    ToggleWatch,
    /// Open a single-gremlin log viewer overlay.
    WatchSingle(String),
    /// Open an interactive debug session overlay.
    Debug(String),
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
                              // Split with shlex to preserve quoted arguments containing spaces.
    let parts =
        shlex::split(body).unwrap_or_else(|| body.split_whitespace().map(String::from).collect());
    let cmd = parts.first().map(|s| s.as_str()).unwrap_or("");
    let args: Vec<&str> = parts.iter().skip(1).map(|s| s.as_str()).collect();

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
        "launch" => {
            let definition = args.first().copied().unwrap_or("");
            if definition.is_empty() {
                return Some(CommandResult::Lines(vec![
                    "> /launch".to_string(),
                    "usage: /launch <definition> [--key value ...]".to_string(),
                ]));
            }

            let raw_args: Vec<String> = args[1..].iter().map(|s| s.to_string()).collect();
            if let Err(e) = validate_launch_args(&raw_args) {
                return Some(CommandResult::Lines(vec![
                    "> /launch".to_string(),
                    "usage: /launch <definition> [--key value ...]".to_string(),
                    e,
                ]));
            }

            Some(CommandResult::SocketOp {
                op: "launch".to_string(),
                payload: serde_json::json!({
                    "op": "launch",
                    "definition": definition,
                    "args": raw_args,
                }),
            })
        }
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
        "watch" => {
            if let Some(id) = args.first() {
                Some(CommandResult::WatchSingle(id.to_string()))
            } else {
                Some(CommandResult::ToggleWatch)
            }
        }
        "debug" => {
            let id = args.first().copied().unwrap_or("");
            if id.is_empty() {
                return Some(CommandResult::Lines(vec![
                    "> /debug".to_string(),
                    "usage: /debug <id>".to_string(),
                ]));
            }
            Some(CommandResult::Debug(id.to_string()))
        }
        "model" => Some(CommandResult::Lines(vec![format!(
            "default client: {}",
            gremlins::config::global_config()
                .ok()
                .and_then(|c| c.default_client().map(String::from))
                .unwrap_or_else(|| "(not configured)".to_string())
        )])),
        _ => Some(CommandResult::Lines(vec![format!(
            "unknown command: /{cmd} — type /help for available commands"
        )])),
    }
}

/// Validate `--key value` pairs using the same syntax as the CLI `launch`
/// subcommand. The daemon performs full validation against the definition's
/// bootstrap sources; this only catches malformed argument syntax locally.
fn validate_launch_args(raw: &[String]) -> Result<(), String> {
    let mut i = 0;
    while i < raw.len() {
        let key = raw[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected --key value, got {}", raw[i]))?;
        if key.is_empty() {
            return Err("-- with no key name".to_string());
        }
        i += 1;
        if i < raw.len() && !raw[i].starts_with("--") {
            i += 1;
        } else {
            return Err(format!(
                "--{key} requires a value (found {})",
                if i < raw.len() {
                    &raw[i]
                } else {
                    "end of arguments"
                }
            ));
        }
    }
    Ok(())
}

fn help_text() -> Vec<String> {
    vec![
        "available commands:".to_string(),
        "  /ls              — list gremlins".to_string(),
        "  /launch <definition> — start a gremlin".to_string(),
        "  /info <id>       — show gremlin info".to_string(),
        "  /stop <id>       — stop a gremlin".to_string(),
        "  /resume <id>     — resume a gremlin".to_string(),
        "  /log <id>        — show gremlin log".to_string(),
        "  /watch           — toggle the gremlins watch table".to_string(),
        "  /watch <id>      — open a single-gremlin log viewer".to_string(),
        "  /debug <id>      — open an interactive debug session".to_string(),
        "  /model           — show default client".to_string(),
        "  /chat            — chat is always active (type directly)".to_string(),
        "  /clear, /new     — restart chat".to_string(),
        "  /help            — show this help".to_string(),
        "  /quit            — exit".to_string(),
        "".to_string(),
        "keybindings:".to_string(),
        "  Ctrl+C           — exit".to_string(),
        "  Ctrl+D (empty)   — exit".to_string(),
        "  Ctrl+L           — redraw screen".to_string(),
        "  Esc              — clear input / cancel in-flight chat".to_string(),
        "  Ctrl+G           — open external editor".to_string(),
        "  Tab              — complete filepaths (Shift+Tab cycles back)".to_string(),
        "".to_string(),
        "scrolling: use terminal scrollback / tmux copy mode".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift check with the chat system prompt's TUI self-knowledge section.
    /// `/help` and the agent prompt must enumerate the same command names.
    #[test]
    fn help_text_lists_all_tui_slash_commands() {
        let commands: Vec<String> = help_text()
            .into_iter()
            .filter(|line| line.contains('/'))
            .map(|line| line.trim_start().to_string())
            .collect();

        let help_names = [
            "/ls", "/launch", "/info", "/stop", "/resume", "/log", "/watch", "/debug", "/model",
            "/chat", "/clear", "/new", "/help", "/quit",
        ];

        for cmd in help_names {
            assert!(
                commands.iter().any(|l| l.contains(cmd)),
                "/help must list {cmd}"
            );
        }
    }

    #[test]
    fn help_text_editor_hint_describes_real_external_editor() {
        let text = help_text().join("\n");
        assert!(text.contains("Ctrl+G") && text.contains("external editor"));
        assert!(!text.contains("(stub)"));
    }
}
