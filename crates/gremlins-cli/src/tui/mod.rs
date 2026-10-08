pub mod app;
pub mod chat;
pub mod client;
pub mod commands;
pub mod editor;
pub mod ui;

use std::io;
use std::sync::Arc;

use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    backend::CrosstermBackend,
    widgets::{Paragraph, Widget},
    Terminal, TerminalOptions, Viewport,
};
use tokio::sync::mpsc;

use app::{App, STREAMING_HEIGHT};
use chat::ChatGremlin;
use commands::{dispatch, CommandResult};
use editor::open_editor;
use ui::render;

/// RAII guard that restores terminal state on drop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Self {
        enable_raw_mode().expect("failed to enable raw mode");
        execute!(io::stdout(), cursor::Hide).ok();
        TerminalGuard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(io::stdout(), cursor::Show).ok();
        println!();
    }
}

/// Initialise the terminal, run the event loop, and restore on exit.
pub async fn run() {
    let _guard = TerminalGuard::enter();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(2 + STREAMING_HEIGHT),
        },
    )
    .expect("failed to create terminal");

    if let Err(e) = run_app(&mut terminal).await {
        eprintln!("tui error: {e}");
    }
}

/// Insert a transcript line above the inline viewport via Ratatui's
/// `insert_before`. This correctly tracks viewport position as lines
/// push it down, and writes the line as normal terminal text that
/// becomes part of the terminal's scrollback history.
fn transcript_line(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    line: &str,
) -> io::Result<()> {
    let text = line.to_string();
    terminal.insert_before(1, |buf| {
        Paragraph::new(text.as_str()).render(buf.area, buf);
    })
}

async fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::new();

    // ── Connect to the daemon socket ──────────────────────────────
    let (client, mut event_rx, mut raw_rx) = match client::connect().await {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("gremlins: cannot connect to daemon: {e}");
            transcript_line(terminal, &msg)?;
            transcript_line(terminal, "")?;
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
            transcript_line(terminal, &msg)?;
            app.push_line(&msg);
        }
    }

    // ── Crossterm event channel (blocking thread → async) ─────────
    let (ct_tx, mut ct_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if ct_tx.send(ev).is_err() {
                break;
            }
        }
    });

    // ── Channel for socket-op results ─────────────────────────────
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<Vec<String>>();

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
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
    transcript_line(terminal, banner)?;
    app.push_line(banner);

    // ── Start chat agent ──────────────────────────────────────────
    let (mut chat_event_rx, _chat_started) = match ChatGremlin::start().await {
        Ok((chat, rx)) => {
            app.chat = Some(chat);
            let msg = "chat agent ready";
            transcript_line(terminal, msg)?;
            app.push_line(msg);
            (Some(rx), true)
        }
        Err(e) => {
            let msg = format!("chat: {e}");
            transcript_line(terminal, &msg)?;
            app.push_line(&msg);
            (None, false)
        }
    };
    app.push_line("");

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
                            // If a chat agent is active, cancel the current turn
                            // and relaunch a fresh session.
                            if let Some(ref chat) = app.chat {
                                chat.cancel();
                                // The Ended event will clear app.chat and chat_event_rx.
                                // After that, start a fresh session.
                                match ChatGremlin::start().await {
                                    Ok((new_chat, rx)) => {
                                        app.chat = Some(new_chat);
                                        chat_event_rx = Some(rx);
                                        let msg = "chat agent restarted";
                                        transcript_line(terminal, msg)?;
                                        app.push_line(msg);
                                    }
                                    Err(e) => {
                                        let msg = format!("chat restart failed: {e}");
                                        transcript_line(terminal, &msg)?;
                                        app.push_line(&msg);
                                    }
                                }
                            } else {
                                break;
                            }
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
                            // In inline mode, force a full viewport redraw.
                            // The transcript is terminal scrollback — tmux/copy-mode owns it.
                            terminal.clear()?;
                        }
                        KeyCode::Char('g')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            open_editor(&mut app);
                        }
                        KeyCode::Esc => {
                            app.input.clear();
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
                                transcript_line(terminal, &echo)?;
                                app.push_line(&echo);

                                match result {
                                    CommandResult::Lines(lines) => {
                                        for line in &lines {
                                            transcript_line(terminal, line)?;
                                            app.push_line(line);
                                        }
                                    }
                                    CommandResult::Clear => {
                                        app.clear_output();
                                        terminal.clear()?;
                                    }
                                    CommandResult::Quit => {
                                        break;
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
                                // Echo user message immediately.
                                let you = format!("You: {input}");
                                transcript_line(terminal, &you)?;
                                app.push_line(&you);
                                if let Some(ref chat) = app.chat {
                                    chat.talk(&input);
                                } else {
                                    let msg = "no agent configured — type /help for commands";
                                    transcript_line(terminal, msg)?;
                                    app.push_line(msg);
                                }
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
                        // In inline mode, autoresize handles viewport repositioning.
                        // The transcript is terminal scrollback — nothing to redraw.
                    }
                    _ => {}
                }
            }

            // ── Daemon events ─────────────────────────────────
            Some(event) = event_rx.recv() => {
                match event {
                    gremlins::executor::DaemonEvent::RunStarted { id, definition, stage } => {
                        let msg = format!("[run started] {id} ({definition}) stage={stage}");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                        app.on_run_started(id, definition, stage);
                    }
                    gremlins::executor::DaemonEvent::RunCompleted { id, exit_code } => {
                        let msg = format!("[run completed] {id} (exit {exit_code})");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                        app.on_run_completed(id);
                    }
                    gremlins::executor::DaemonEvent::RunFailed { id, exit_code, error } => {
                        let err_detail = error.as_deref().unwrap_or("");
                        let msg = if err_detail.is_empty() {
                            format!("[run failed] {id} (exit {exit_code})")
                        } else {
                            format!("[run failed] {id} (exit {exit_code}): {err_detail}")
                        };
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                        app.on_run_failed(id);
                    }
                    gremlins::executor::DaemonEvent::RunStopped { id } => {
                        let msg = format!("[run stopped] {id}");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                        app.on_run_stopped(id);
                    }
                    gremlins::executor::DaemonEvent::StageTransition { id, stage } => {
                        let msg = format!("[{id}] stage → {stage}");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                    }
                    gremlins::executor::DaemonEvent::LogLine { id, line } => {
                        if app.following_log.as_deref() == Some(&id) {
                            transcript_line(terminal, &line)?;
                            app.push_line(&line);
                        }
                    }
                    gremlins::executor::DaemonEvent::Bail { id, reason } => {
                        let msg = format!("[{id}] bail: {reason}");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                    }
                }
            }

            // ── Raw lines (non-event JSON from the main socket) ─
            Some(raw) = raw_rx.recv() => {
                if raw.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = raw
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let full = format!("error: {msg}");
                    transcript_line(terminal, &full)?;
                    app.push_line(&full);
                }
            }

            // ── Log follow lines (from dedicated connection) ──
            Some(raw) = log_rx.recv() => {
                if raw.get("type").and_then(|v| v.as_str()) == Some("log_line") {
                    if let Some(line) = raw.get("line").and_then(|v| v.as_str()) {
                        if app.following_log.is_some() {
                            transcript_line(terminal, line)?;
                            app.push_line(line);
                        }
                    }
                } else if raw.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = raw
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    let full = format!("error: {msg}");
                    transcript_line(terminal, &full)?;
                    app.push_line(&full);
                    app.following_log = None;
                }
            }

            // ── Socket-op results ─────────────────────────────
            Some(lines) = result_rx.recv() => {
                for line in &lines {
                    transcript_line(terminal, line)?;
                    app.push_line(line);
                }
            }

            // ── Chat events ────────────────────────────────
            Some(event) = async {
                match &mut chat_event_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    chat::ChatEvent::Ready => {}
                    chat::ChatEvent::StreamChunk(text) => {
                        app.stream_text.push_str(&text);
                        app.streamed_visible_text = true;
                    }
                    chat::ChatEvent::ReasoningChunk(text) => {
                        for ch in text.chars() {
                            if app.reasoning_line_start {
                                app.stream_text.push_str("  ");
                                app.reasoning_line_start = false;
                            }
                            app.stream_text.push(ch);
                            if ch == '\n' {
                                app.reasoning_line_start = true;
                            }
                        }
                    }
                    chat::ChatEvent::TurnComplete { turn: _, text, tool_calls } => {
                        // Commit streaming content to scrollback.
                        if !app.stream_text.is_empty() {
                            let stream_lines: Vec<String> = app.stream_text.lines().map(String::from).collect();
                            for line in &stream_lines {
                                transcript_line(terminal, line)?;
                                app.push_line(line);
                            }
                        }
                        // Commit assembled text if it wasn't captured in the stream.
                        if !text.is_empty() && !app.streamed_visible_text {
                            for line in text.lines() {
                                transcript_line(terminal, line)?;
                                app.push_line(line);
                            }
                        }
                        app.stream_text.clear();
                        app.streamed_visible_text = false;
                        app.reasoning_line_start = true;
                        app.turn_committed = true;

                        for tc in &tool_calls {
                            let line = format!("  {tc}");
                            transcript_line(terminal, &line)?;
                            app.push_line(&line);
                        }
                        // Auto-continue when the turn had tool calls
                        // but produced no text — the agent needs another
                        // turn to process results and respond.
                        if text.is_empty() && !tool_calls.is_empty() {
                            if let Some(ref chat) = app.chat {
                                chat.continue_turn();
                            }
                        }
                    }
                    chat::ChatEvent::Done { text, .. } => {
                        // If TurnComplete already committed the stream, skip.
                        if app.turn_committed {
                            app.turn_committed = false;
                            continue;
                        }
                        // Commit streaming content to scrollback.
                        if !app.stream_text.is_empty() {
                            let stream_lines: Vec<String> = app.stream_text.lines().map(String::from).collect();
                            for line in &stream_lines {
                                transcript_line(terminal, line)?;
                                app.push_line(line);
                            }
                        }
                        if !text.is_empty() && !app.streamed_visible_text {
                            for line in text.lines() {
                                transcript_line(terminal, line)?;
                                app.push_line(line);
                            }
                        }
                        app.stream_text.clear();
                        app.streamed_visible_text = false;
                        app.reasoning_line_start = true;
                    }
                    chat::ChatEvent::Ended { reason } => {
                        app.stream_text.clear();
                        app.streamed_visible_text = false;
                        app.reasoning_line_start = true;
                        app.turn_committed = false;
                        let msg = format!("chat ended: {reason}");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                        app.chat = None;
                        chat_event_rx = None;
                    }
                    chat::ChatEvent::Error(msg) => {
                        app.stream_text.clear();
                        app.streamed_visible_text = false;
                        app.reasoning_line_start = true;
                        app.turn_committed = false;
                        let full = format!("chat error: {msg}");
                        transcript_line(terminal, &full)?;
                        app.push_line(&full);
                        app.chat = None;
                        chat_event_rx = None;
                    }
                }
            }
        }
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
