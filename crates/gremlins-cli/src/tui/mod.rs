pub mod app;
pub mod chat;
pub mod client;
pub mod commands;
pub mod editor;
pub mod ui;
pub mod widgets;

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    Terminal, TerminalOptions, Viewport,
};
use tokio::sync::mpsc;

use app::App;
use chat::{send_message, ChatEvent};
use commands::{dispatch, CommandResult};
use editor::open_editor;
use ui::render;
use widgets::{DynamicWidget, SplitWidget};

/// RAII guard that restores terminal state on drop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Self {
        enable_raw_mode().expect("failed to enable raw mode");
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide).ok();
        TerminalGuard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        execute!(io::stdout(), cursor::Show, LeaveAlternateScreen).ok();
        disable_raw_mode().ok();
    }
}

/// Freeze the widget, push response lines and stream tail to scrollback,
/// commit user+assistant pair to conversation history, and clear the widget.
///
/// This is the single freeze-and-commit path used by Done, Ended, Error,
/// and Esc.
fn freeze_and_commit(app: &mut App) {
    if let Some(ref mut widget) = app.widget {
        widget.flush_partial();
        let (response_lines, stream_tail) = widget.freeze();
        app.extend_scrollback(stream_tail);
        app.extend_scrollback(response_lines);
    }
    app.widget = None;

    // Commit user+assistant pair to conversation history.
    let user_msg = std::mem::take(&mut app.pending_user_message);
    if !user_msg.is_empty() {
        app.conversation_history
            .push(serde_json::json!({"role": "user", "content": user_msg}));
    }
    let response_text = std::mem::take(&mut app.current_response);
    if !response_text.is_empty() {
        app.conversation_history
            .push(serde_json::json!({"role": "assistant", "content": response_text}));
    }
    app.active_request = false;
}

/// Initialise the terminal, run the event loop, and restore on exit.
pub async fn run() {
    // Refuse to start the TUI when stdout is not a terminal.
    if !std::io::stdout().is_terminal() {
        eprintln!("gremlins: stdout is not a terminal — use `gremlins ls` to list gremlins");
        return;
    }

    let _guard = TerminalGuard::enter();

    // Fullscreen viewport on the alternate screen buffer. The frame area
    // resizes automatically on terminal resize.
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .expect("failed to create terminal");

    if let Err(e) = run_app(&mut terminal).await {
        eprintln!("tui error: {e}");
    }
}

async fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::new();

    // ── Connect to the daemon socket ──────────────────────────────
    let (client, mut event_rx, mut raw_rx) = match client::connect().await {
        Ok(c) => c,
        Err(e) => {
            return Err(io::Error::new(io::ErrorKind::NotConnected, e));
        }
    };
    let client = Arc::new(client);

    // ── Initial state snapshot ────────────────────────────────────
    match client.send_request(serde_json::json!({"op": "ls"})).await {
        Ok(resp) => {
            if let Some(gremlins) = resp.get("gremlins").and_then(|v| v.as_array()) {
                for entry in gremlins {
                    if let (Some(id), Some(status)) = (
                        entry.get("id").and_then(|v| v.as_str()),
                        entry.get("status").and_then(|v| v.as_str()),
                    ) {
                        app.active_runs.insert(id.to_string(), status.to_string());
                    }
                }
            }
        }
        Err(e) => {
            let msg = format!("error fetching initial state: {e}");
            app.push_scrollback(msg);
        }
    }

    // ── Crossterm event channel (polling task → async) ─────────
    // Uses poll + sleep instead of blocking read so that the event
    // loop is never starved by a background reader holding the stdin lock.
    let (ct_tx, mut ct_rx) = mpsc::unbounded_channel::<Event>();
    tokio::spawn(async move {
        loop {
            match crossterm::event::poll(Duration::from_millis(20)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if ct_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(_) => break,
            }
        }
    });

    // ── Channel for socket-op results ─────────────────────────────
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<Vec<String>>();

    // ── Channel for run-snapshot refreshes (after event_lagged) ───
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<HashMap<String, String>>();

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // ── Chat task handle (aborted on Esc to stop the agent) ───────
    let mut chat_task: Option<tokio::task::JoinHandle<()>> = None;
    // Oneshot sender to cancel the socket-reader spawned inside
    // send_message(). Firing this drops write_half so the daemon
    // sees EOF and stops the agent immediately.
    let mut chat_cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    /// Drop-guard that aborts a JoinHandle on drop, ensuring the dedicated
    /// log-follow socket is closed even when the outer forwarding task is
    /// cancelled via `abort()`.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    // ── Startup banner ────────────────────────────────────────────
    let banner = "gremlins tui — type /help for commands, Ctrl+C to exit";
    app.push_scrollback(banner.to_string());

    // ── Channel for chat events from the current (or most recent) message.
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<ChatEvent>();

    // ── Event loop ────────────────────────────────────────────────
    loop {
        let gremlin_count = app.gremlin_count_str();
        let project_name = app.project_name_str().to_string();
        terminal.draw(|frame| render(frame, &app, &gremlin_count, &project_name))?;

        tokio::select! {
            // ── Crossterm events ──────────────────────────────
            Some(ev) = ct_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('c')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            break;
                        }
                        KeyCode::Char('d')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            if app.input.is_empty() {
                                break;
                            }
                        }
                        KeyCode::Char('l')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            // Force a full viewport redraw.
                            terminal.clear()?;
                        }
                        KeyCode::Char('g')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            open_editor(&mut app);
                        }
                        KeyCode::Esc => {
                            app.input.clear();
                            // If a chat request is in-flight, abort it to
                            // stop the agent loop and all tool calls.
                            // Fire the cancel oneshot first so the inner
                            // socket-reader drops write_half — the daemon
                            // sees EOF and stops the agent immediately.
                            if app.active_request {
                                if let Some(tx) = chat_cancel_tx.take() {
                                    let _ = tx.send(());
                                }
                                if let Some(handle) = chat_task.take() {
                                    handle.abort();
                                }
                                freeze_and_commit(&mut app);
                            }
                        }
                        KeyCode::Enter => {
                            let input = std::mem::take(&mut app.input);
                            if input.is_empty() {
                                continue;
                            }

                            // Stop log following on any new command.
                            if let Some(handle) = log_follow_handle.take() {
                                handle.abort();
                                app.following_log = None;
                            }

                            if let Some(result) = dispatch(&input) {
                                let echo = format!("> {input}");
                                let prompt_style = Style::default().fg(Color::Cyan);
                                app.push_scrollback_styled(echo, prompt_style);

                                match result {
                                    CommandResult::Lines(lines) => {
                                        for line in &lines {
                                            app.push_scrollback(line.clone());
                                        }
                                    }
                                    CommandResult::RestartChat => {
                                        // Freeze any active widget.
                                        if app.widget.is_some() {
                                            freeze_and_commit(&mut app);
                                        }
                                        // Abort in-flight chat task and
                                        // cancel the inner socket reader.
                                        if let Some(tx) = chat_cancel_tx.take() {
                                            let _ = tx.send(());
                                        }
                                        if let Some(handle) = chat_task.take() {
                                            handle.abort();
                                        }
                                        app.conversation_history.clear();
                                        app.current_response.clear();
                                        app.input.clear();
                                        app.active_request = false;
                                        app.pending_user_message.clear();
                                    }
                                    CommandResult::Quit => {
                                        break;
                                    }
                                    CommandResult::ShowHistory => {
                                        if app.conversation_history.is_empty() {
                                            app.push_scrollback("(no history)".to_string());
                                        } else {
                                            let mut history_lines: Vec<String> = Vec::new();
                                            for (i, entry) in app.conversation_history.iter().enumerate() {
                                                let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("");
                                                let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
                                                // Truncate content for display, using char
                                                // boundaries to avoid panicking on multi-byte
                                                // UTF-8 characters.
                                                let preview: String = if content.chars().count() > 60 {
                                                    let truncated: String =
                                                        content.chars().take(57).collect();
                                                    format!("{truncated}...")
                                                } else {
                                                    content.to_string()
                                                };
                                                let line = format!("  [{i}] {role}: {preview}");
                                                history_lines.push(line);
                                            }
                                            for line in history_lines {
                                                app.push_scrollback(line);
                                            }
                                        }
                                    }
                                    CommandResult::TruncateHistory(idx) => {
                                        if idx >= app.conversation_history.len() {
                                            let msg = format!(
                                                "invalid index {idx} — history has {} entries",
                                                app.conversation_history.len()
                                            );
                                            app.push_scrollback(msg);
                                        } else {
                                            app.conversation_history.truncate(idx);
                                            let msg = format!("history truncated to {idx} entries");
                                            app.push_scrollback(msg);
                                        }
                                    }
                                    CommandResult::SocketOp { op, payload } => {
                                        if op == "log" {
                                            // Open a dedicated follow connection.
                                            if let Some(id) = payload
                                                .get("id")
                                                .and_then(|v| v.as_str())
                                                .map(String::from)
                                            {
                                                app.following_log = Some(id.clone());
                                                let log_tx = log_tx.clone();
                                                let follow_task = tokio::spawn(async move {
                                                    match client::follow_log(&id).await {
                                                        Ok((mut rx, handle)) => {
                                                            // Abort the inner reader on drop so
                                                            // the dedicated socket is always closed.
                                                            let _guard = AbortOnDrop(handle);
                                                            while let Some(line) = rx.recv().await {
                                                                if log_tx.send(line).is_err() {
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            let _ = log_tx.send(
                                                                serde_json::json!({
                                                                    "type": "error",
                                                                    "message": e,
                                                                }),
                                                            );
                                                        }
                                                    }
                                                });
                                                log_follow_handle = Some(follow_task);
                                            }
                                        } else {
                                            // Request-response op.
                                            let c = Arc::clone(&client);
                                            let tx = result_tx.clone();
                                            tokio::spawn(async move {
                                                let lines = match c.send_request(payload).await {
                                                    Ok(resp) => format_socket_response(&op, &resp),
                                                    Err(e) => vec![format!("error: {e}")],
                                                };
                                                let _ = tx.send(lines);
                                            });
                                        }
                                    }
                                }
                            } else {
                                // Plain text — send to chat agent.
                                // Guard against concurrent requests.
                                if app.active_request {
                                    let msg = "a request is already in progress — wait for the response";
                                    app.push_scrollback(msg.to_string());
                                    continue;
                                }
                                // Echo user prompt to scrollback.
                                let echo = format!("> {input}");
                                let prompt_style = Style::default().fg(Color::Cyan);
                                app.push_scrollback_styled(echo, prompt_style);

                                // Create a SplitWidget and push the initial "thinking..." line.
                                let reason_style = Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC);
                                let mut widget = SplitWidget::new();
                                widget.push_stream(Line::from(Span::styled(
                                    "  thinking...",
                                    reason_style,
                                )));
                                app.widget = Some(widget);

                                // Force an immediate frame so "thinking..."
                                // appears without waiting for the next event.
                                terminal.draw(|frame| render(frame, &app, &gremlin_count, &project_name))?;

                                app.active_request = true;
                                app.pending_user_message = input.clone();
                                let history = app.conversation_history.clone();
                                let chat_tx = chat_tx.clone();
                                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                                chat_cancel_tx = Some(cancel_tx);
                                chat_task = Some(tokio::spawn(async move {
                                    match send_message(&input, &history, cancel_rx).await {
                                        Ok(mut rx) => {
                                            while let Some(event) = rx.recv().await {
                                                if chat_tx.send(event).is_err() {
                                                    break;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let _ = chat_tx.send(ChatEvent::Error(e));
                                        }
                                    }
                                }));
                            }
                        }
                        KeyCode::Backspace => {
                            app.input.pop();
                        }
                        KeyCode::Char(ch) => {
                            app.input.push(ch);
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => {
                        // Fullscreen viewport resizes automatically; no
                        // state to update.
                    }
                    _ => {}
                }
            }

            // ── Daemon events ─────────────────────────────────
            Some(event) = event_rx.recv() => {
                match event {
                    gremlins::executor::DaemonEvent::RunStarted { id, definition, stage } => {
                        let msg = format!("[run started] {id} ({definition}) stage={stage}");
                        app.push_scrollback(msg);
                        app.on_run_started(id, definition, stage);
                    }
                    gremlins::executor::DaemonEvent::RunCompleted { id, exit_code } => {
                        let msg = format!("[run completed] {id} (exit {exit_code})");
                        app.push_scrollback(msg);
                        app.on_run_completed(id);
                    }
                    gremlins::executor::DaemonEvent::RunFailed { id, exit_code, error } => {
                        let err_detail = error.as_deref().unwrap_or("");
                        let msg = if err_detail.is_empty() {
                            format!("[run failed] {id} (exit {exit_code})")
                        } else {
                            format!("[run failed] {id} (exit {exit_code}): {err_detail}")
                        };
                        app.push_scrollback(msg);
                        app.on_run_failed(id);
                    }
                    gremlins::executor::DaemonEvent::RunStopped { id } => {
                        let msg = format!("[run stopped] {id}");
                        app.push_scrollback(msg);
                        app.on_run_stopped(id);
                    }
                    gremlins::executor::DaemonEvent::StageTransition { id, stage } => {
                        let msg = format!("[{id}] stage → {stage}");
                        app.push_scrollback(msg);
                    }
                    gremlins::executor::DaemonEvent::LogLine { id, line } => {
                        if app.following_log.as_deref() == Some(&id) {
                            app.push_scrollback(line);
                        }
                    }
                    gremlins::executor::DaemonEvent::Bail { id, reason } => {
                        let msg = format!("[{id}] bail: {reason}");
                        app.push_scrollback(msg);
                    }
                }
            }

            // ── Raw lines (non-event JSON from the main socket) ─
            Some(raw) = raw_rx.recv() => {
                if raw.get("type").and_then(|v| v.as_str()) == Some("event_lagged") {
                    // Lifecycle events were dropped — the info-bar run
                    // count may be stale. Refresh from the daemon.
                    let c = Arc::clone(&client);
                    let tx = snapshot_tx.clone();
                    tokio::spawn(async move {
                        if let Ok(resp) = c.send_request(serde_json::json!({"op": "ls"})).await {
                            let mut runs = HashMap::new();
                            if let Some(gremlins) =
                                resp.get("gremlins").and_then(|v| v.as_array())
                            {
                                for entry in gremlins {
                                    if let (Some(id), Some(status)) = (
                                        entry.get("id").and_then(|v| v.as_str()),
                                        entry.get("status").and_then(|v| v.as_str()),
                                    ) {
                                        runs.insert(
                                            id.to_string(),
                                            status.to_string(),
                                        );
                                    }
                                }
                            }
                            let _ = tx.send(runs);
                        }
                    });
                } else if raw.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = raw
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let full = format!("error: {msg}");
                    app.push_scrollback(full);
                }
            }

            // ── Run-snapshot refresh (after event_lagged) ─────
            Some(runs) = snapshot_rx.recv() => {
                app.active_runs = runs;
            }

            // ── Log follow lines (from dedicated connection) ──
            Some(raw) = log_rx.recv() => {
                if raw.get("type").and_then(|v| v.as_str()) == Some("log_line") {
                    if let Some(line) = raw.get("line").and_then(|v| v.as_str()) {
                        if app.following_log.is_some() {
                            app.push_scrollback(line.to_string());
                        }
                    }
                } else if raw.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = raw
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let full = format!("error: {msg}");
                    app.push_scrollback(full);
                    app.following_log = None;
                }
            }

            // ── Socket-op results ─────────────────────────────
            Some(lines) = result_rx.recv() => {
                for line in &lines {
                    app.push_scrollback(line.clone());
                }
            }

            // ── Chat events ────────────────────────────────
            Some(event) = chat_rx.recv() => {
                match event {
                    ChatEvent::StreamChunk(text) => {
                        app.current_response.push_str(&text);
                        if let Some(ref mut widget) = app.widget {
                            widget.push_response_text(&text);
                        }
                    }
                    ChatEvent::ReasoningChunk(text) => {
                        let reason_style = Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC);
                        if let Some(ref mut widget) = app.widget {
                            widget.push_stream_text(&text, reason_style);
                        }
                    }
                    ChatEvent::ToolResult { name, output } => {
                        let tool_style = Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC);
                        if let Some(ref mut widget) = app.widget {
                            if output.is_empty() {
                                widget.push_stream(Line::from(Span::styled(
                                    format!("  {name}: (empty)"),
                                    tool_style,
                                )));
                            } else {
                                for (i, line) in output.lines().enumerate() {
                                    if i == 0 {
                                        widget.push_stream(Line::from(Span::styled(
                                            format!("  {name}: {line}"),
                                            tool_style,
                                        )));
                                    } else {
                                        widget.push_stream(Line::from(Span::styled(
                                            format!("        {line}"),
                                            tool_style,
                                        )));
                                    }
                                }
                            }
                        }
                    }
                    ChatEvent::TurnComplete { .. } => {
                        // No-op: widget keeps rendering; no state flags to toggle.
                    }
                    ChatEvent::Done { text, .. } => {
                        if app.widget.is_none() {
                            continue;
                        }
                        // Reconcile accumulated stream with canonical text.
                        // After a broadcast lag, current_response is nonempty
                        // but missing chunks; the canonical Done.text is the
                        // authoritative full response.
                        if !text.is_empty()
                            && app.current_response != text
                        {
                            app.current_response = text.clone();
                            if let Some(ref mut widget) = app.widget {
                                widget.replace_response(&text);
                            }
                        }
                        freeze_and_commit(&mut app);
                        chat_task = None;
                        chat_cancel_tx = None;
                    }
                    ChatEvent::Ended { reason } => {
                        if app.widget.is_none() {
                            continue;
                        }
                        freeze_and_commit(&mut app);
                        chat_task = None;
                        chat_cancel_tx = None;
                        let msg = format!("chat ended: {reason}");
                        app.push_scrollback(msg);
                    }
                    ChatEvent::Error(msg) => {
                        if app.widget.is_none() {
                            continue;
                        }
                        freeze_and_commit(&mut app);
                        chat_task = None;
                        chat_cancel_tx = None;
                        let full = format!("chat error: {msg}");
                        app.push_scrollback(full);
                    }
                }
            }
        }
    }

    // ── Exit ──────────────────────────────────────────────────────

    // Freeze any active widget.
    if app.widget.is_some() {
        freeze_and_commit(&mut app);
    }

    // Abort any lingering log follow on exit.
    if let Some(handle) = log_follow_handle.take() {
        handle.abort();
    }

    // Close the persistent socket connection.
    client.shutdown().await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Response formatting (mirrors the CLI output for each op)
// ---------------------------------------------------------------------------

fn format_socket_response(op: &str, resp: &serde_json::Value) -> Vec<String> {
    if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
        let msg = resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return vec![format!("error: {msg}")];
    }

    match op {
        "ls" => format_ls_response(resp),
        "info" => {
            vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
        }
        "stop" => {
            let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let status = resp.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let message = resp.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if !message.is_empty() {
                vec![format!("gremlin {id}: {message}")]
            } else {
                vec![format!("gremlin {id} {status}")]
            }
        }
        "resume" => {
            let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("");
            vec![id.to_string()]
        }
        _ => {
            vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
        }
    }
}

fn format_ls_response(resp: &serde_json::Value) -> Vec<String> {
    let gremlins = match resp.get("gremlins").and_then(|v| v.as_array()) {
        Some(g) => g,
        None => return vec!["(no gremlins)".to_string()],
    };

    let headers = ["ID", "STATUS", "STAGE", "DATE", "PROJECT", "LAUNCH"];
    let mut rows: Vec<Vec<String>> = Vec::new();

    for entry in gremlins {
        rows.push(vec![
            entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("stage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("started_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            entry
                .get("launch")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ]);
    }

    rows.sort_by(|a, b| a[3].cmp(&b[3]));

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let mut lines: Vec<String> = Vec::new();

    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let w = widths.get(i).copied().unwrap_or(0);
            format!("{h:<w$}")
        })
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(header_line);

    for row in &rows {
        let line = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(0);
                format!("{cell:<w$}")
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(line);
    }

    lines
}
