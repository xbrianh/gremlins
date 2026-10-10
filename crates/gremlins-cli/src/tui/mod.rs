pub mod app;
pub mod chat;
pub mod client;
pub mod commands;
pub mod editor;
pub mod overlays;
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
use tokio::io::BufReader;
use tokio::sync::mpsc;

use app::{ActiveRun, App, Overlay};
use chat::{send_message, ChatEvent};
use commands::{dispatch, CommandResult};
use editor::open_editor;
use gremlins::executor::socket;
use ui::render;
use widgets::WidgetEvent;

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

/// Finish the active widget, commit user+assistant pair to conversation
/// history, and clear active-request state.
fn finish_and_commit(app: &mut App, events: Vec<WidgetEvent>) {
    app.finish_active(events);

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

/// Check whether there is an active (streaming) widget in the transcript.
fn has_active_widget(app: &App) -> bool {
    app.transcript.last().is_some_and(|w| !w.is_expandable())
}

/// Abort the dedicated log-follow task for the single-gremlin overlay.
fn abort_overlay_log_follow(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        handle.abort();
    }
}

/// Route a submitted line from the debug overlay prompt.
///
/// The debug socket is opened when `/debug <id>` is dispatched; this
/// function only forwards subsequent prompt submissions to the daemon
/// over the already-established session.
fn handle_overlay_prompt_submit(app: &mut App, cmd_tx: &mpsc::UnboundedSender<serde_json::Value>) {
    let Some(input) = app.take_overlay_prompt() else {
        return;
    };

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return;
    }

    // Echo the command in the overlay scrollback.
    app.push_overlay_debug_output(format!("debug> {trimmed}"));

    // Reject submissions while the socket is still coming up.
    if app.debug_connecting() {
        app.push_overlay_debug_output("debug: session not ready yet".to_string());
        return;
    }

    // Reject further submissions once the session has ended.
    if !app.debug_connected() {
        app.push_overlay_debug_output("(session ended)".to_string());
        return;
    }

    // Map the input to a daemon debug op.
    let cmd = if trimmed == "/exit" || trimmed == "/continue" {
        serde_json::json!({"op": "quit"})
    } else if let Some(rest) = trimmed.strip_prefix("/quit ") {
        serde_json::json!({"op": "bail", "reason": rest.trim()})
    } else if trimmed == "/quit" {
        serde_json::json!({"op": "bail", "reason": "operator stopped"})
    } else {
        serde_json::json!({"op": "talk", "text": trimmed})
    };

    let _ = cmd_tx.send(cmd);
}

/// Events received from the daemon over a debug session socket.
enum DebugEvent {
    Ready {
        id: String,
    },
    TurnComplete {
        #[allow(dead_code)]
        turn: usize,
        text: String,
        tool_calls: Vec<String>,
    },
    Done {
        text: String,
    },
    Ended {
        reason: String,
    },
    StreamChunk {
        text: String,
    },
    ReasoningChunk {
        text: String,
    },
    ToolResult {
        name: String,
        output: String,
    },
    Error(String),
    Disconnected,
}

/// Parse a daemon debug-protocol JSON line into a [`DebugEvent`].
fn parse_debug_event(value: &serde_json::Value) -> DebugEvent {
    match value.get("type").and_then(|v| v.as_str()) {
        Some("ready") => DebugEvent::Ready {
            id: value
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("turn_complete") => DebugEvent::TurnComplete {
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
        Some("done") => DebugEvent::Done {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("ended") => DebugEvent::Ended {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("stream_chunk") => DebugEvent::StreamChunk {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("reasoning_chunk") => DebugEvent::ReasoningChunk {
            text: value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("tool_result") => DebugEvent::ToolResult {
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
        Some("error") => DebugEvent::Error(
            value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string(),
        ),
        _ => DebugEvent::Error(format!("unexpected event: {value}")),
    }
}

/// Daemon broadcast events (run lifecycle, log lines, etc.) are forwarded
/// to every connection, including the debug socket. They are not part of
/// the debug protocol and are filtered out.
fn is_daemon_broadcast(value: &serde_json::Value) -> bool {
    match value.get("type").and_then(|v| v.as_str()) {
        Some(t) if t.starts_with("run_") => true,
        Some("event") | Some("stage_transition") | Some("log_line") | Some("bail") => true,
        _ => false,
    }
}

/// Open a fresh debug session socket for `id` and spawn a reader task.
///
/// Returns a sender for forwarding commands (`talk`/`quit`/`bail`/`continue`)
/// to the daemon, a receiver for incoming [`DebugEvent`] values, and the
/// task handle (abort it to drop the write half and close the socket).
async fn start_debug_session(
    id: String,
) -> Result<
    (
        mpsc::UnboundedSender<serde_json::Value>,
        mpsc::UnboundedReceiver<DebugEvent>,
        tokio::task::JoinHandle<()>,
    ),
    String,
> {
    let state_root = gremlins::config::state_root();
    let stream = socket::connect_socket(&state_root).await?;
    let (read_half, mut write_half) = stream.into_split();

    let request = serde_json::json!({ "op": "debug", "id": id });
    socket::write_json_line(&mut write_half, &request).await?;

    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<DebugEvent>();

    let handle = tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            if socket::write_json_line(&mut write_half, &cmd).await.is_err() {
                                let _ = evt_tx.send(DebugEvent::Disconnected);
                                break;
                            }
                        }
                        None => break,
                    }
                }
                result = socket::read_json_line(&mut reader) => {
                    match result {
                        Ok(Some(value)) => {
                            if is_daemon_broadcast(&value) {
                                continue;
                            }
                            let evt = parse_debug_event(&value);
                            let is_terminal = matches!(
                                &evt,
                                DebugEvent::Done { .. } | DebugEvent::Ended { .. }
                            );
                            let _ = evt_tx.send(evt);
                            if is_terminal {
                                break;
                            }
                        }
                        Ok(None) | Err(_) => {
                            let _ = evt_tx.send(DebugEvent::Disconnected);
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok((cmd_tx, evt_rx, handle))
}

/// Await the next debug event, pending forever when no session is active.
async fn recv_debug(rx: &mut Option<mpsc::UnboundedReceiver<DebugEvent>>) -> Option<DebugEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
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
                    app.upsert_run_from_ls(entry);
                }
            }
        }
        Err(e) => {
            let msg = format!("error fetching initial state: {e}");
            app.push_system_line(msg);
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
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<HashMap<String, ActiveRun>>();

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Dedicated follow for the single-gremlin log overlay.
    let mut overlay_log_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut overlay_log_following: Option<String> = None;
    // ── Chat task handle (aborted on Esc to stop the agent) ───────
    let mut chat_task: Option<tokio::task::JoinHandle<()>> = None;
    // Oneshot sender to cancel the socket-reader spawned inside
    // send_message(). Firing this drops write_half so the daemon
    // sees EOF and stops the agent immediately.
    let mut chat_cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // ── Debug session state ────────────────────────────────────────
    // Command sender for the active debug socket (talk/quit/bail/continue).
    let mut debug_cmd_tx: Option<mpsc::UnboundedSender<serde_json::Value>> = None;
    // Incoming debug events from the socket reader task.
    let mut debug_evt_rx: Option<mpsc::UnboundedReceiver<DebugEvent>> = None;
    // Handle for the debug socket reader task (aborted on Esc).
    let mut debug_task: Option<tokio::task::JoinHandle<()>> = None;

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
    app.push_system_line(banner.to_string());

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
                            if app.overlay.as_ref().is_some_and(Overlay::has_prompt) {
                                app.overlay = None;
                                continue;
                            }
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
                        KeyCode::Char('o')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            app.toggle_expand_all();
                        }
                        KeyCode::Esc => {
                            // Blocking overlays (single-watch, debug) dismiss first.
                            if app.dismiss_blocking_overlay() {
                                abort_overlay_log_follow(&mut overlay_log_handle);
                                overlay_log_following = None;
                                // Drop the debug socket so the daemon sees EOF.
                                if let Some(handle) = debug_task.take() {
                                    handle.abort();
                                }
                                debug_cmd_tx = None;
                                debug_evt_rx = None;
                                continue;
                            }
                            // Then the non-blocking watch table.
                            if app.dismiss_watch_overlay() {
                                continue;
                            }

                            app.input.clear();
                            // If a chat request is in-flight, abort it to
                            // stop the agent loop and all tool calls.
                            if app.active_request {
                                if let Some(tx) = chat_cancel_tx.take() {
                                    let _ = tx.send(());
                                }
                                if let Some(handle) = chat_task.take() {
                                    handle.abort();
                                }
                                finish_and_commit(&mut app, Vec::new());
                            }
                        }
                        KeyCode::Enter => {
                            // Debug overlay owns a prompt: submit into its
                            // scrollback instead of the main input bar.
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                if let Some(cmd_tx) = &debug_cmd_tx {
                                    handle_overlay_prompt_submit(&mut app, cmd_tx);
                                } else {
                                    // Consume the prompt so it doesn't linger.
                                    let _ = app.take_overlay_prompt();
                                    app.push_overlay_debug_output(
                                        "(session ended)".to_string(),
                                    );
                                }
                                continue;
                            }

                            let input = std::mem::take(&mut app.input);
                            if input.is_empty() {
                                continue;
                            }

                            // A blocking single-watch overlay only accepts
                            // slash commands; plain chat text is ignored.
                            if app.overlay.as_ref().is_some_and(Overlay::is_blocking)
                                && !input.trim_start().starts_with('/')
                            {
                                continue;
                            }

                            // Record the submission for Up/Down history.
                            app.push_history(input.clone());

                            // Stop log following on any new command.
                            if let Some(handle) = log_follow_handle.take() {
                                handle.abort();
                                app.following_log = None;
                            }

                            if let Some(result) = dispatch(&input) {
                                match result {
                                    CommandResult::ToggleWatch => {
                                        if app.overlay.as_ref().is_some_and(|o| !matches!(o, Overlay::Watch)) {
                                            abort_overlay_log_follow(&mut overlay_log_handle);
                                            overlay_log_following = None;
                                        }
                                        match app.overlay {
                                            Some(Overlay::Watch) => app.overlay = None,
                                            _ => app.overlay = Some(Overlay::Watch),
                                        }
                                    }
                                    CommandResult::WatchSingle(id) => {
                                        // Close any existing overlay log follow first.
                                        abort_overlay_log_follow(&mut overlay_log_handle);
                                        overlay_log_following = Some(id.clone());
                                        app.open_watch_single(id.clone());
                                        let log_tx = log_tx.clone();
                                        let follow_id = id.clone();
                                        let follow_task = tokio::spawn(async move {
                                            match client::follow_log(&follow_id).await {
                                                Ok((mut rx, handle)) => {
                                                    let _guard = AbortOnDrop(handle);
                                                    while let Some(raw) = rx.recv().await {
                                                        let Some(line) = raw
                                                            .get("line")
                                                            .and_then(|v| v.as_str())
                                                        else {
                                                            continue;
                                                        };
                                                        let tagged = serde_json::json!({
                                                            "type": "overlay_log",
                                                            "id": follow_id,
                                                            "line": line,
                                                        });
                                                        if log_tx.send(tagged).is_err() {
                                                            break;
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    let _ = log_tx.send(
                                                        serde_json::json!({
                                                            "type": "overlay_error",
                                                            "id": follow_id,
                                                            "message": e,
                                                        }),
                                                    );
                                                }
                                            }
                                        });
                                        overlay_log_handle = Some(follow_task);
                                    }
                                    CommandResult::Debug(id) => {
                                        abort_overlay_log_follow(&mut overlay_log_handle);
                                        overlay_log_following = None;
                                        app.open_debug(id.clone());
                                        // Open a fresh socket and spawn the reader.
                                        match start_debug_session(id.clone()).await {
                                            Ok((cmd_tx, evt_rx, handle)) => {
                                                debug_cmd_tx = Some(cmd_tx);
                                                debug_evt_rx = Some(evt_rx);
                                                debug_task = Some(handle);
                                            }
                                            Err(e) => {
                                                app.push_overlay_debug_output(format!("error: {e}"));
                                            }
                                        }
                                    }
                                    CommandResult::Lines(lines) => {
                                        let mut styled: Vec<(String, Style)> = Vec::new();
                                        let prompt_style = Style::default().fg(Color::Cyan);
                                        styled.push((format!("> {input}"), prompt_style));
                                        for line in &lines {
                                            styled.push((line.clone(), Style::default()));
                                        }
                                        app.push_system(styled);
                                    }
                                    CommandResult::RestartChat => {
                                        // Finish any active widget.
                                        if has_active_widget(&app) {
                                            finish_and_commit(&mut app, Vec::new());
                                        }
                                        // Abort in-flight chat task.
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
                                            app.push_system_line("(no history)".to_string());
                                        } else {
                                            let mut styled: Vec<(String, Style)> = Vec::new();
                                            for (i, entry) in app.conversation_history.iter().enumerate() {
                                                let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("");
                                                let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
                                                let preview: String = if content.chars().count() > 60 {
                                                    let truncated: String =
                                                        content.chars().take(57).collect();
                                                    format!("{truncated}...")
                                                } else {
                                                    content.to_string()
                                                };
                                                let line = format!("  [{i}] {role}: {preview}");
                                                styled.push((line, Style::default()));
                                            }
                                            app.push_system(styled);
                                        }
                                    }
                                    CommandResult::TruncateHistory(idx) => {
                                        if idx >= app.conversation_history.len() {
                                            let msg = format!(
                                                "invalid index {idx} — history has {} entries",
                                                app.conversation_history.len()
                                            );
                                            app.push_system_line(msg);
                                        } else {
                                            app.conversation_history.truncate(idx);
                                            let msg = format!("history truncated to {idx} entries");
                                            app.push_system_line(msg);
                                        }
                                    }
                                    CommandResult::SocketOp { op, payload } => {
                                        if op == "log" {
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
                                if app.active_request {
                                    let msg = "a request is already in progress — wait for the response";
                                    app.push_system_line(msg.to_string());
                                    continue;
                                }

                                // Push ActivePromptWidget with the prompt.
                                app.push_active_prompt(input.clone());

                                // Push initial "thinking..." line.
                                let reason_style = Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC);
                                if let Some(w) = app.active_mut() {
                                    w.push_stream_line(Line::from(Span::styled(
                                        "  thinking...",
                                        reason_style,
                                    )));
                                }

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
                            if let Some(prompt) = app.overlay_prompt_mut() {
                                prompt.pop();
                            } else {
                                app.input.pop();
                            }
                        }
                        KeyCode::Char(ch) => {
                            if let Some(prompt) = app.overlay_prompt_mut() {
                                prompt.push(ch);
                            } else {
                                app.input.push(ch);
                                // Typing a fresh character resets history navigation.
                                app.history_cursor = None;
                            }
                        }
                        KeyCode::Up => {
                            // History navigation only applies to the main input bar.
                            if !app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                if let Some(entry) = app.history_up().map(|s| s.to_string()) {
                                    app.input = entry;
                                }
                            }
                        }
                        KeyCode::Down if !app.overlay.as_ref().is_some_and(|o| o.has_prompt())
                            // Only replace the buffer when navigating; when the
                            // cursor is already `None` (fresh line), leave it.
                            && app.history_cursor.is_some() => {
                                app.input = app.history_down().unwrap_or("").to_string();
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
                        app.push_system_line(msg);
                        app.on_run_started(id, definition, stage);
                    }
                    gremlins::executor::DaemonEvent::RunCompleted { id, exit_code } => {
                        let msg = format!("[run completed] {id} (exit {exit_code})");
                        app.push_system_line(msg);
                        app.on_run_completed(id);
                    }
                    gremlins::executor::DaemonEvent::RunFailed { id, exit_code, error } => {
                        let err_detail = error.as_deref().unwrap_or("");
                        let msg = if err_detail.is_empty() {
                            format!("[run failed] {id} (exit {exit_code})")
                        } else {
                            format!("[run failed] {id} (exit {exit_code}): {err_detail}")
                        };
                        app.push_system_line(msg);
                        app.on_run_failed(id);
                    }
                    gremlins::executor::DaemonEvent::RunStopped { id } => {
                        let msg = format!("[run stopped] {id}");
                        app.push_system_line(msg);
                        app.on_run_stopped(id);
                    }
                    gremlins::executor::DaemonEvent::StageTransition { id, stage } => {
                        let msg = format!("[{id}] stage → {stage}");
                        app.push_system_line(msg);
                        app.on_stage_transition(id, stage);
                    }
                    gremlins::executor::DaemonEvent::LogLine { id, line } => {
                        if app.following_log.as_deref() == Some(&id) {
                            app.push_system_line(line.clone());
                        }
                        if overlay_log_following.as_deref() == Some(&id) {
                            app.push_overlay_log_line(line);
                        }
                    }
                    gremlins::executor::DaemonEvent::Bail { id, reason } => {
                        let msg = format!("[{id}] bail: {reason}");
                        app.push_system_line(msg);
                    }
                }
            }

            // ── Raw lines (non-event JSON from the main socket) ─
            Some(raw) = raw_rx.recv() => {
                if raw.get("type").and_then(|v| v.as_str()) == Some("event_lagged") {
                    let c = Arc::clone(&client);
                    let tx = snapshot_tx.clone();
                    let fallback_project = app.project_name_str().to_string();
                    tokio::spawn(async move {
                        if let Ok(resp) = c.send_request(serde_json::json!({"op": "ls"})).await {
                            let mut runs = HashMap::new();
                            if let Some(gremlins) =
                                resp.get("gremlins").and_then(|v| v.as_array())
                            {
                                for entry in gremlins {
                                    if let Some(id) =
                                        entry.get("id").and_then(|v| v.as_str())
                                    {
                                        let project = entry
                                            .get("project")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let project = if project.is_empty() {
                                            fallback_project.clone()
                                        } else {
                                            project
                                        };
                                        runs.insert(
                                            id.to_string(),
                                            ActiveRun {
                                                status: entry
                                                    .get("status")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                stage: entry
                                                    .get("stage")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                definition: entry
                                                    .get("definition")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                project,
                                            },
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
                    app.push_system_line(full);
                }
            }

            // ── Run-snapshot refresh (after event_lagged) ─────
            Some(mut runs) = snapshot_rx.recv() => {
                for run in runs.values_mut() {
                    if run.project.is_empty() {
                        run.project = app.project_name.clone();
                    }
                }
                app.active_runs = runs;
            }

            // ── Log follow lines (from dedicated connections) ──
            Some(raw) = log_rx.recv() => {
                match raw.get("type").and_then(|v| v.as_str()) {
                    Some("log_line") => {
                        if let Some(line) = raw.get("line").and_then(|v| v.as_str()) {
                            if app.following_log.is_some() {
                                app.push_system_line(line.to_string());
                            }
                        }
                    }
                    Some("overlay_log") => {
                        // Scope the line to the currently followed gremlin so
                        // queued messages from an aborted follow can't leak
                        // into a newly opened viewer for another id.
                        if let (Some(id), Some(line)) = (
                            raw.get("id").and_then(|v| v.as_str()),
                            raw.get("line").and_then(|v| v.as_str()),
                        ) {
                            if overlay_log_following.as_deref() == Some(id) {
                                app.push_overlay_log_line(line.to_string());
                            }
                        }
                    }
                    Some("overlay_error") => {
                        if let (Some(id), Some(message)) = (
                            raw.get("id").and_then(|v| v.as_str()),
                            raw.get("message").and_then(|v| v.as_str()),
                        ) {
                            if overlay_log_following.as_deref() == Some(id) {
                                app.push_overlay_log_line(format!("error: {message}"));
                                overlay_log_following = None;
                            }
                        }
                    }
                    Some("error") => {
                        let msg = raw
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        let full = format!("error: {msg}");
                        if app.following_log.is_some() {
                            app.push_system_line(full.clone());
                            app.following_log = None;
                        }
                    }
                    _ => {}
                }
            }

            // ── Socket-op results ─────────────────────────────
            Some(lines) = result_rx.recv() => {
                let styled: Vec<(String, Style)> = lines
                    .into_iter()
                    .map(|s| (s, Style::default()))
                    .collect();
                app.push_system(styled);
            }

            // ── Chat events ────────────────────────────────
            Some(event) = chat_rx.recv() => {
                match event {
                    ChatEvent::StreamChunk(text) => {
                        app.current_response.push_str(&text);
                        if let Some(w) = app.active_mut() {
                            w.push_response_text(&text);
                        }
                    }
                    ChatEvent::ReasoningChunk(text) => {
                        let reason_style = Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC);
                        if let Some(w) = app.active_mut() {
                            w.push_stream_text(&text, reason_style);
                        }
                    }
                    ChatEvent::ToolResult { name, output } => {
                        if let Some(w) = app.active_mut() {
                            w.push_tool_result(&name, &output);
                        }
                    }
                    ChatEvent::TurnComplete { .. } => {
                        // No-op.
                    }
                    ChatEvent::Done { text, usage } => {
                        if !has_active_widget(&app) {
                            continue;
                        }
                        // Reconcile accumulated stream with canonical text.
                        if !text.is_empty()
                            && app.current_response != text
                        {
                            app.current_response = text.clone();
                            if let Some(w) = app.active_mut() {
                                w.replace_response(&text);
                            }
                        }
                        let mut events = Vec::new();
                        if let Some(usage) = usage {
                            if let (Some(prompt), Some(completion)) = (
                                usage.get("input_tokens").and_then(|v| v.as_u64()),
                                usage.get("output_tokens").and_then(|v| v.as_u64()),
                            ) {
                                let cache_read = usage
                                    .get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                events.push(WidgetEvent::TokenUsage {
                                    prompt: prompt as usize,
                                    completion: completion as usize,
                                    cache_read: cache_read as usize,
                                });
                            }
                        }
                        finish_and_commit(&mut app, events);
                        chat_task = None;
                        chat_cancel_tx = None;
                    }
                    ChatEvent::Ended { reason } => {
                        if !has_active_widget(&app) {
                            continue;
                        }
                        finish_and_commit(&mut app, Vec::new());
                        chat_task = None;
                        chat_cancel_tx = None;
                        let msg = format!("chat ended: {reason}");
                        app.push_system_line(msg);
                    }
                    ChatEvent::Error(msg) => {
                        if !has_active_widget(&app) {
                            continue;
                        }
                        finish_and_commit(&mut app, Vec::new());
                        chat_task = None;
                        chat_cancel_tx = None;
                        let full = format!("chat error: {msg}");
                        app.push_system_line(full);
                    }
                }
            }

            // ── Debug session events ──────────────────────────
            Some(event) = recv_debug(&mut debug_evt_rx) => {
                match event {
                    DebugEvent::Ready { id } => {
                        app.mark_debug_ready();
                        app.push_overlay_debug_output(format!("connected to gremlin {id}"));
                    }
                    DebugEvent::TurnComplete { text, tool_calls, .. } => {
                        if !text.is_empty() {
                            app.push_overlay_debug_output(text);
                        }
                        for tc in &tool_calls {
                            app.push_overlay_debug_output(format!("  [tool: {tc}]"));
                        }
                        app.push_overlay_debug_output(
                            "debug: turn complete — agent paused".to_string(),
                        );
                    }
                    DebugEvent::Done { text } => {
                        if !text.is_empty() {
                            app.push_overlay_debug_output(text);
                        }
                        app.push_overlay_debug_output("debug: agent called Done".to_string());
                        app.set_debug_connected(false);
                        debug_task = None;
                        debug_cmd_tx = None;
                        debug_evt_rx = None;
                    }
                    DebugEvent::Ended { reason } => {
                        app.push_overlay_debug_output(format!("debug: session ended ({reason})"));
                        app.set_debug_connected(false);
                        debug_task = None;
                        debug_cmd_tx = None;
                        debug_evt_rx = None;
                    }
                    DebugEvent::StreamChunk { text } => {
                        app.push_overlay_debug_output(text);
                    }
                    DebugEvent::ReasoningChunk { text } => {
                        app.push_overlay_debug_output(text);
                    }
                    DebugEvent::ToolResult { name, output } => {
                        app.push_overlay_debug_output(format!("[tool: {name}] {output}"));
                    }
                    DebugEvent::Error(msg) => {
                        app.push_overlay_debug_output(format!("debug: error: {msg}"));
                    }
                    DebugEvent::Disconnected => {
                        app.push_overlay_debug_output("debug: disconnected".to_string());
                        app.set_debug_connected(false);
                        debug_task = None;
                        debug_cmd_tx = None;
                        debug_evt_rx = None;
                    }
                }
            }
        }
    }

    // ── Exit ──────────────────────────────────────────────────────

    // Finish any active widget.
    if has_active_widget(&app) {
        finish_and_commit(&mut app, Vec::new());
    }

    // Abort any lingering log follow on exit.
    if let Some(handle) = log_follow_handle.take() {
        handle.abort();
    }

    // Abort any lingering debug session on exit.
    if let Some(handle) = debug_task.take() {
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
