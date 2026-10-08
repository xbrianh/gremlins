pub mod app;
pub mod chat;
pub mod client;
pub mod commands;
pub mod editor;
pub mod ui;

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
    Terminal, TerminalOptions, Viewport,
};
use tokio::sync::mpsc;

use app::{App, STREAMING_HEIGHT};
use chat::{send_message, ChatEvent};
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

/// Recreate the terminal with a new inline viewport height.
/// Used to collapse the streaming area when idle and expand it when
/// a turn is active.
fn set_viewport_height(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    height: u16,
) -> io::Result<()> {
    terminal.clear()?;
    let _old_backend =
        std::mem::replace(terminal.backend_mut(), CrosstermBackend::new(io::stdout()));
    // _old_backend is dropped here, releasing the stdout handle.
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .expect("failed to recreate terminal");
    Ok(())
}

/// Initialise the terminal, run the event loop, and restore on exit.
pub async fn run() {
    // Refuse to start the TUI when stdout is not a terminal.
    if !std::io::stdout().is_terminal() {
        eprintln!("gremlins: stdout is not a terminal — use `gremlins ls` to list gremlins");
        return;
    }

    let _guard = TerminalGuard::enter();

    // Start with a minimal viewport (input + info only).
    // The viewport expands to include the streaming area when a turn is active.
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(2),
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

/// Like [`transcript_line`] but applies a ratatui [`Style`] to the line.
fn transcript_styled(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    line: &str,
    style: Style,
) -> io::Result<()> {
    let span = Span::styled(line, style);
    terminal.insert_before(1, |buf| {
        Paragraph::new(Line::from(span)).render(buf.area, buf);
    })
}

// ── Thinking indicator helpers ───────────────────────────────────

/// Show the "thinking..." placeholder in the streaming area.
fn show_thinking(app: &mut App) {
    app.streaming_active = true;
    if !app.showing_thinking {
        app.reasoning_stream = "thinking...".to_string();
        app.response_stream.clear();
        app.showing_thinking = true;
        app.streaming_rows = 1;
    }
}

/// Clear the "thinking..." placeholder when the first real chunk arrives.
fn clear_thinking(app: &mut App) {
    if app.showing_thinking {
        app.reasoning_stream.clear();
        app.showing_thinking = false;
    }
}

/// Mark streaming as complete — clear streams and deactivate.
fn finish_streaming(app: &mut App) {
    app.prompt.clear();
    app.prompt_lines = 0;
    app.reasoning_stream.clear();
    app.response_stream.clear();
    app.streamed_visible_text = false;
    app.turn_committed = false;
    app.showing_thinking = false;
    app.streaming_active = false;
    app.response_area_open = false;
    app.streaming_rows = 0;
    app.response_rows = 0;
}

/// Viewport height: content height, capped at terminal height.
fn active_viewport_height(app: &App, term_h: u16) -> u16 {
    let response_h = if app.response_area_open { app.response_rows } else { 0 };
    let needed = 2 + app.prompt_lines + app.streaming_rows + response_h;
    needed.min(term_h)
}

/// Compute prompt line count (wrapping at ~80 cols) and snapshot it.
fn snapshot_prompt_lines(prompt: &str) -> u16 {
    let chars = prompt.chars().count();
    let wrapped = (chars + 2).div_ceil(78); // +2 for "> " prefix
    let explicit = prompt.lines().count();
    (wrapped.max(explicit) as u16).max(1)
}

/// Grow streaming_rows to match reasoning_stream line count, capped at
/// STREAMING_HEIGHT. Resizes viewport on change.
fn sync_streaming_rows(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    viewport_height: &mut u16,
    term_h: u16,
) -> io::Result<()> {
    let actual = (app.reasoning_stream.lines().count() as u16).max(1);
    let target = actual.min(STREAMING_HEIGHT);
    if target != app.streaming_rows {
        app.streaming_rows = target;
        let needed = active_viewport_height(app, term_h);
        if *viewport_height != needed {
            set_viewport_height(terminal, needed)?;
            *viewport_height = needed;
        }
    }
    Ok(())
}

/// Grow response_rows to match response_stream. No per-section cap —
/// the total viewport is capped at terminal height by active_viewport_height.
fn sync_response_rows(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    viewport_height: &mut u16,
    term_h: u16,
) -> io::Result<()> {
    let actual = (app.response_stream.lines().count() as u16).max(1);
    if actual != app.response_rows {
        app.response_rows = actual;
        let needed = active_viewport_height(app, term_h);
        if *viewport_height != needed {
            set_viewport_height(terminal, needed)?;
            *viewport_height = needed;
        }
    }
    Ok(())
}

/// Flush the current viewport content (prompt + last streaming lines +
/// response) to scrollback, then clear state and collapse viewport.
fn flush_and_collapse(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    viewport_height: &mut u16,
    reason_style: Style,
) -> io::Result<()> {
    // Prompt.
    if !app.prompt.is_empty() {
        let prompt_line = format!("> {}", app.prompt);
        transcript_line(terminal, &prompt_line)?;
        app.push_line(&prompt_line);
    }
    // Last visible streaming lines only (not the entire history).
    if !app.reasoning_stream.is_empty() {
        let all_lines: Vec<String> = app.reasoning_stream.lines().map(String::from).collect();
        let skip = all_lines.len().saturating_sub(app.streaming_rows as usize);
        for line in &all_lines[skip..] {
            let styled = format!("  {line}");
            transcript_styled(terminal, &styled, reason_style)?;
            app.push_line(&styled);
        }
    }
    // Response.
    if !app.response_stream.is_empty() {
        let response_lines: Vec<String> =
            app.response_stream.lines().map(String::from).collect();
        for line in &response_lines {
            transcript_line(terminal, line)?;
            app.push_line(line);
        }
    }
    finish_streaming(app);
    if *viewport_height > 2 {
        set_viewport_height(terminal, 2)?;
        *viewport_height = 2;
    }
    Ok(())
}

async fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::new();
    let mut viewport_height: u16 = 2; // input + info only; expands when streaming
    let term_h = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);

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

    // ── Crossterm event channel (polling task → async) ─────────
    // Uses poll + sleep instead of blocking read so that ratatui's
    // inline-viewport cursor-position query (which also reads stdin)
    // is never starved by a background reader holding the stdin lock.
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

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // ── Chat task handle (aborted on Esc to stop the agent) ───────
    let mut chat_task: Option<tokio::task::JoinHandle<()>> = None;
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
                            app.input.clear();
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
                            // If a chat request is in-flight, abort it to
                            // stop the agent loop and all tool calls.
                            if app.active_request {
                                if let Some(handle) = chat_task.take() {
                                    handle.abort();
                                }
                                // Flush partial content so it stays visible.
                                flush_and_collapse(
                                    &mut app, terminal, &mut viewport_height,
                                    Style::default()
                                        .fg(Color::DarkGray)
                                        .add_modifier(Modifier::ITALIC),
                                )?;
                                app.turn_committed = false;
                                app.current_response.clear();
                                app.pending_user_message.clear();
                                app.active_request = false;
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
                                transcript_line(terminal, &echo)?;
                                app.push_line(&echo);

                                match result {
                                    CommandResult::Lines(lines) => {
                                        for line in &lines {
                                            transcript_line(terminal, line)?;
                                            app.push_line(line);
                                        }
                                    }
                                    CommandResult::RestartChat => {
                                        finish_streaming(&mut app);
                                        app.prompt.clear();
                                        app.clear_output();
                                        app.turn_committed = false;
                                        app.conversation_history.clear();
                                        app.current_response.clear();
                                        app.input.clear();
                                        if viewport_height > 2 {
                                            set_viewport_height(terminal, 2)?;
                                            viewport_height = 2;
                                        }
                                        terminal.clear()?;
                                        let msg = "chat history cleared";
                                        transcript_line(terminal, msg)?;
                                        app.push_line(msg);
                                    }
                                    CommandResult::Quit => {
                                        break;
                                    }
                                    CommandResult::ShowHistory => {
                                        if app.conversation_history.is_empty() {
                                            transcript_line(terminal, "(no history)")?;
                                            app.push_line("(no history)");
                                        } else {
                                            let mut lines_to_push: Vec<String> = Vec::new();
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
                                                lines_to_push.push(line);
                                            }
                                            for line in &lines_to_push {
                                                transcript_line(terminal, line)?;
                                                app.push_line(line);
                                            }
                                        }
                                    }
                                    CommandResult::TruncateHistory(idx) => {
                                        if idx >= app.conversation_history.len() {
                                            let msg = format!(
                                                "invalid index {idx} — history has {} entries",
                                                app.conversation_history.len()
                                            );
                                            transcript_line(terminal, &msg)?;
                                            app.push_line(&msg);
                                        } else {
                                            app.conversation_history.truncate(idx);
                                            let msg = format!("history truncated to {idx} entries");
                                            transcript_line(terminal, &msg)?;
                                            app.push_line(&msg);
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
                                    transcript_line(terminal, msg)?;
                                    app.push_line(msg);
                                    continue;
                                }
                                // Store prompt for display in the viewport.
                                // Don't echo to scrollback — the viewport shows it.
                                app.prompt = input.clone();
                                app.prompt_lines = snapshot_prompt_lines(&app.prompt);

                                // Do NOT push to conversation_history yet —
                                // only commit the user+assistant pair on Done.
                                // Send prior history (without this message) to the daemon.

                                show_thinking(&mut app);
                                // Expand viewport for streaming (prompt + streaming + response).
                                let needed = active_viewport_height(&app, term_h);
                                if viewport_height < needed {
                                    set_viewport_height(terminal, needed)?;
                                    viewport_height = needed;
                                }
                                // Force an immediate frame so "thinking..."
                                // appears without waiting for the next event.
                                terminal.draw(|frame| render(frame, &app, &gremlin_count, &project_name))?;

                                app.active_request = true;
                                app.pending_user_message = input.clone();
                                let history = app.conversation_history.clone();
                                let chat_tx = chat_tx.clone();
                                chat_task = Some(tokio::spawn(async move {
                                    match send_message(&input, &history).await {
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
            Some(event) = chat_rx.recv() => {
                let reason_style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);

                match event {
                    ChatEvent::StreamChunk(text) => {
                        clear_thinking(&mut app);
                        if !app.response_area_open {
                            app.response_area_open = true;
                        }
                        app.response_stream.push_str(&text);
                        app.current_response.push_str(&text);
                        app.streamed_visible_text = true;
                        sync_response_rows(&mut app, terminal, &mut viewport_height, term_h)?;
                    }
                    ChatEvent::ReasoningChunk(text) => {
                        clear_thinking(&mut app);
                        app.reasoning_stream.push_str(&text);
                        sync_streaming_rows(&mut app, terminal, &mut viewport_height, term_h)?;
                    }
                    ChatEvent::ToolResult { name, output } => {
                        clear_thinking(&mut app);
                        // Append to reasoning stream so it renders live
                        // in the streaming viewport alongside thought.
                        // Lines are unprefixed — TurnComplete adds "  " on flush.
                        if output.is_empty() {
                            app.reasoning_stream.push_str(&format!("{name}: (empty)\n"));
                        } else {
                            for (i, line) in output.lines().enumerate() {
                                if i == 0 {
                                    app.reasoning_stream.push_str(&format!("{name}: {line}\n"));
                                } else {
                                    app.reasoning_stream.push_str(&format!("      {line}\n"));
                                }
                            }
                        }
                        sync_streaming_rows(&mut app, terminal, &mut viewport_height, term_h)?;
                    }
                    ChatEvent::TurnComplete { turn: _, text, tool_calls: _ } => {
                        clear_thinking(&mut app);
                        // If we haven't streamed visible text yet, push the turn text.
                        if !text.is_empty() && !app.streamed_visible_text {
                            app.response_stream.push_str(&text);
                            app.current_response.push_str(&text);
                            if !app.response_area_open {
                                app.response_area_open = true;
                            }
                            sync_response_rows(&mut app, terminal, &mut viewport_height, term_h)?;
                        }
                        // Keep everything in the viewport — flush nothing to
                        // scrollback until Done, so content never jumps above
                        // the current prompt.
                        app.turn_committed = true;
                    }
                    ChatEvent::Done { text, .. } => {
                        clear_thinking(&mut app);
                        if !text.is_empty() && !app.streamed_visible_text {
                            app.response_stream.push_str(&text);
                            app.current_response.push_str(&text);
                        }
                        flush_and_collapse(&mut app, terminal, &mut viewport_height, reason_style)?;
                        // Commit user+assistant pair to conversation history.
                        let user_msg = std::mem::take(&mut app.pending_user_message);
                        if !user_msg.is_empty() {
                            app.conversation_history.push(serde_json::json!({"role": "user", "content": user_msg}));
                        }
                        let response_text = std::mem::take(&mut app.current_response);
                        if !response_text.is_empty() {
                            app.conversation_history.push(serde_json::json!({"role": "assistant", "content": response_text}));
                        }
                        app.active_request = false;
                        chat_task = None;
                    }
                    ChatEvent::Ended { reason } => {
                        finish_streaming(&mut app);
                        app.turn_committed = false;
                        app.current_response.clear();
                        app.pending_user_message.clear();
                        app.active_request = false;
                        chat_task = None;
                        if viewport_height > 2 {
                            set_viewport_height(terminal, 2)?;
                            viewport_height = 2;
                        }
                        let msg = format!("chat ended: {reason}");
                        transcript_line(terminal, &msg)?;
                        app.push_line(&msg);
                    }
                    ChatEvent::Error(msg) => {
                        finish_streaming(&mut app);
                        app.turn_committed = false;
                        app.current_response.clear();
                        app.pending_user_message.clear();
                        app.active_request = false;
                        chat_task = None;
                        if viewport_height > 2 {
                            set_viewport_height(terminal, 2)?;
                            viewport_height = 2;
                        }
                        let full = format!("chat error: {msg}");
                        transcript_line(terminal, &full)?;
                        app.push_line(&full);
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
