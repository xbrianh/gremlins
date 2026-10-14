pub mod app;
pub mod buffer;
pub mod chat;
mod chat_context;
pub mod client;
pub mod commands;
pub mod completion;
pub mod editor;
pub mod markdown;
pub mod motif;
pub mod screen;
pub mod theme;
pub mod widgets;

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use tokio::io::BufReader;
use tokio::sync::mpsc;

use app::{ActiveRun, App, Overlay};
use chat::ChatEvent;
use commands::{dispatch, CommandResult};
use editor::{open_editor, ReaderChannels, SuspendHandle};
use gremlins::executor::socket;
use screen::LiveArea;
use widgets::{DebugWidget, WatchSingleWidget, WatchTableWidget};

/// Abort the dedicated log-follow task for the single-gremlin overlay.
fn abort_overlay_log_follow(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        handle.abort();
    }
}

/// Overall deadline for the whole repository-context snapshot. Each git
/// subprocess is individually capped, but several run in sequence; this
/// bounds the total so a hung `git` cannot stall request construction for
/// tens of seconds.
const CONTEXT_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the `chat` request payload sent to the daemon. Carries the operator's
/// project root so the chat gremlin anchors against the actual project rather
/// than the daemon's own working directory.
///
/// `context` is the repository-context snapshot gathered off the UI thread by
/// the caller (see [`send_chat_input`]); it is prepended to the operator's
/// message here. The daemon passes the fully assembled prompt through without
/// further wrapping.
fn build_chat_request(
    text: &str,
    turn: &str,
    chat_id: Option<&str>,
    project_root: &str,
    context: &str,
    env: &HashMap<String, String>,
) -> serde_json::Value {
    let user_text = if context.is_empty() {
        text.to_string()
    } else {
        format!("{context}\n\n{text}")
    };
    let mut request = serde_json::json!({
        "op": "chat",
        "text": user_text,
        "turn": turn,
        "project_root": project_root,
        "env": env,
    });
    if let Some(id) = chat_id {
        request["id"] = serde_json::Value::String(id.to_string());
    }
    request
}

/// Submit plain-text chat input to the daemon.
///
/// This is the shared tail of the Enter and `Ctrl+G`-editor paths. Caller
/// is responsible for taking the input, recording history, and stopping any
/// log follow first. The caller must have already checked `active_request`.
fn send_chat_input(
    live_area: &mut LiveArea,
    app: &mut App,
    client: &Arc<client::DaemonClient>,
    input: String,
    turn_gen: &mut u64,
    active_turn: &mut Option<String>,
) -> io::Result<()> {
    if app.active_request {
        let msg = "a request is already in progress — wait for the response";
        live_area.push_transcript_line(msg.to_string());
        return Ok(());
    }

    // Start a new turn with the prompt.
    live_area.submit_prompt(input.clone());

    // Force an immediate frame so the pinned prompt appears without waiting
    // for the first daemon event.
    live_area.compose(app)?;

    app.active_request = true;
    *turn_gen += 1;
    let turn_id = turn_gen.to_string();
    *active_turn = Some(turn_id.clone());
    let chat_id = app.chat_id.clone();
    let client_for_chat = Arc::clone(client);
    let chat_tx = client_for_chat.chat_sender();
    // The operator's project root, resolved the same way the info bar does.
    let project_root = gremlins::config::project_root();
    tokio::spawn(async move {
        // Gather the repository-context snapshot off the runtime's worker
        // threads (git subprocesses are blocking) and under one overall
        // deadline, so a hung `git` cannot stall the chat turn or starve the
        // event loop. On timeout or failure the prompt is sent without a
        // context block rather than being dropped.
        let project_root_str = project_root.to_string_lossy().into_owned();
        let context = match tokio::time::timeout(
            CONTEXT_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                chat_context::build_chat_context(&project_root_str)
            }),
        )
        .await
        {
            Ok(Ok(ctx)) => ctx,
            Ok(Err(_)) | Err(_) => String::new(),
        };
        // Forward the TUI process's environment so the chat gremlin's Bash
        // tool runs commands under the operator's environment rather than an
        // empty one. Collected here (on the chat submission path) so it
        // reflects the operator's shell at submit time.
        let env: HashMap<String, String> = std::env::vars().collect();
        let request = build_chat_request(
            &input,
            &turn_id,
            chat_id.as_deref(),
            &project_root.to_string_lossy(),
            &context,
            &env,
        );
        match client_for_chat.send_request(request).await {
            Ok(resp) => {
                // A chat ack is `{"type":"ok",...}`; an error response is
                // `{"type":"error",...}`. Surface a setup failure as a chat
                // error so `active_request` is cleared.
                if resp.get("type").and_then(|v| v.as_str()) == Some("error") {
                    let msg = resp
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    let _ = chat_tx.send(ChatEvent::Error {
                        message: msg,
                        id: None,
                        turn: Some(turn_id.clone()),
                    });
                } else if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                    // The ack carries the gremlin id (created on the first
                    // turn). Route it back to the event loop so `app.chat_id`
                    // is stored before Esc/resubmit can race the terminal
                    // Done/Ended event.
                    let _ = chat_tx.send(ChatEvent::ChatId {
                        id: id.to_string(),
                        turn: Some(turn_id.clone()),
                    });
                }
            }
            Err(e) => {
                let _ = chat_tx.send(ChatEvent::Error {
                    message: e,
                    id: None,
                    turn: Some(turn_id.clone()),
                });
            }
        }
    });

    Ok(())
}

/// Route a submitted line from the debug overlay prompt.
///
/// The debug socket is opened when `/debug <id>` is dispatched; this
/// function only forwards subsequent prompt submissions to the daemon
/// over the already-established session.
fn handle_overlay_prompt_submit(
    live_area: &mut LiveArea,
    cmd_tx: &mpsc::UnboundedSender<serde_json::Value>,
) {
    let Some(input) = live_area.take_debug_prompt() else {
        return;
    };

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return;
    }

    // Echo the command in the overlay scrollback.
    live_area.push_debug_output(format!("debug> {trimmed}"));

    // Reject submissions while the socket is still coming up.
    if live_area.debug_connecting() {
        live_area.push_debug_output("debug: session not ready yet".to_string());
        return;
    }

    // Reject further submissions once the session has ended.
    if !live_area.debug_connected() {
        live_area.push_debug_output("(session ended)".to_string());
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

    // Redirect stderr to tui.log so debug logs persist next to executor.log.
    let log_path = gremlins::config::state_root().join("tui.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::os::fd::IntoRawFd;
        let fd = file.into_raw_fd();
        unsafe { libc::dup2(fd, 2) };
        unsafe { libc::close(fd) };
    }

    let mut live_area = match LiveArea::new() {
        Ok(area) => area,
        Err(e) => {
            log::error!("tui: {e}");
            return;
        }
    };

    if let Err(e) = run_app(&mut live_area).await {
        log::error!("tui: {e}");
    }
}

/// Path to the persisted input history file, under `$state_root/tui/`.
fn history_path() -> std::path::PathBuf {
    gremlins::config::state_root()
        .join("tui")
        .join("history.json")
}

/// Maximum byte size of the history file we are willing to read. A larger
/// file is treated as absent (empty history), guarding against a pathological
/// file exhausting memory before the event loop starts.
const HISTORY_MAX_BYTES: u64 = 1 << 20; // 1 MiB

/// Load the persisted input history (a JSON array of strings, newest-last).
///
/// A missing, unreadable, oversized, or malformed file yields an empty
/// history — never a fatal error and never a transcript line. The result is
/// capped to [`App::HISTORY_CAP`] entries so a valid oversized file cannot
/// bypass the bounded-history policy.
fn load_history() -> Vec<String> {
    let path = history_path();
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(_) => return Vec::new(),
    };
    if data.len() as u64 > HISTORY_MAX_BYTES {
        return Vec::new();
    }
    let mut history: Vec<String> = serde_json::from_slice(&data).unwrap_or_default();
    if history.len() > App::HISTORY_CAP {
        history.drain(..history.len() - App::HISTORY_CAP);
    }
    history
}

/// Persist the input history as a JSON array of strings (newest-last).
///
/// Errors are silently ignored (logged at most); history persistence must
/// never fail the TUI. The write is atomic: data goes to a sibling temp file
/// which is then renamed over the target, so a crash mid-write leaves the
/// previous file intact rather than a truncated one.
fn save_history(history: &[String]) {
    let path = history_path();
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let data = match serde_json::to_string_pretty(history) {
        Ok(data) => data,
        Err(_) => return,
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, data).is_err() {
        log::debug!("tui: failed to write history to {}", tmp.display());
        return;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        log::debug!("tui: failed to rename history into {}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

async fn run_app(live_area: &mut LiveArea) -> io::Result<()> {
    let mut app = App::new();

    // Seed the in-memory history from the persisted file before the event
    // loop starts, so Up/Down recall survives a TUI restart.
    app.input_history = load_history();

    // ── Connect to the daemon socket ──────────────────────────────
    let (client, mut event_rx, mut chat_rx, mut raw_rx) = match client::connect().await {
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
            live_area.push_transcript_line(msg);
        }
    }

    // ── Crossterm event channel (polling task → async) ─────────
    // Uses poll + sleep instead of blocking read so that the event
    // loop is never starved by a background reader holding the stdin lock.
    // The reader can be suspended (parked) while an external editor owns
    // the terminal, so it never races the editor for keystrokes.
    let (suspend_handle, reader) = SuspendHandle::new();
    let (ct_tx, mut ct_rx) = mpsc::unbounded_channel::<Event>();
    {
        let ReaderChannels {
            mut suspend_rx,
            mut resume_rx,
            parked,
            dead,
        } = reader;
        tokio::spawn(async move {
            loop {
                // Park on a suspend request: acknowledge, then wait for the
                // resume signal before polling again.
                if suspend_rx.try_recv().is_ok() {
                    parked.notify_one();
                    if resume_rx.recv().await.is_none() {
                        break;
                    }
                    continue;
                }
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
            dead.notify_one();
        });
    }

    // ── Channel for socket-op results ─────────────────────────────
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<Vec<String>>();

    // ── Channel for run-snapshot refreshes (after event_lagged) ───
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<HashMap<String, ActiveRun>>();

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Dedicated follow for the single-gremlin log overlay.
    let mut overlay_log_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut overlay_log_following: Option<String> = None;
    // ── Chat turn state ────────────────────────────────────────
    // (the chat request is sent from a detached task; we no longer need to
    // abort it, so no JoinHandle is kept)
    // Monotonic turn id, bumped on every submit and every Esc so that a
    // stale turn's terminal events (which arrive after a cancel or a new
    // submit) can be recognised and dropped.
    let mut turn_gen: u64 = 0;
    // The turn id of the currently active turn, if any.
    let mut active_turn: Option<String> = None;
    // Chat IDs explicitly destroyed by /clear — terminal events from
    // these sessions are stale and must not restore them.
    let mut cleared_chat_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
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
    for line in motif::spire_banner() {
        live_area.push_styled_line(line);
    }
    live_area.push_styled_line(motif::tagline());

    // ── Event loop ────────────────────────────────────────────────
    loop {
        let (cols, _rows) = live_area.size();
        app.width = cols;
        live_area.compose(&app)?;

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
                                live_area.pop_overlay();
                                // Drop the debug socket so the daemon sees EOF.
                                if let Some(handle) = debug_task.take() {
                                    handle.abort();
                                }
                                debug_cmd_tx = None;
                                debug_evt_rx = None;
                                continue;
                            }
                            if app.input.is_empty() {
                                break;
                            }
                        }
                        KeyCode::Char('l')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            live_area.invalidate();
                        }
                        KeyCode::Char('g')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            // Never suspend the TUI while a chat turn is
                            // in flight; the editor can't even be given a
                            // meaningful prompt to submit yet.
                            if app.active_request {
                                live_area.push_transcript_line(
                                    "a request is already in progress — wait for the response"
                                        .to_string(),
                                );
                                continue;
                            }

                            match open_editor(&app.input, &suspend_handle).await {
                                Ok(outcome) => {
                                    // `open_editor` returns the reader-resume
                                    // guard so we can drain the event channel
                                    // while the reader is still parked. This
                                    // guarantees no handoff keystrokes are
                                    // either replayed afterwards or discarded
                                    // accidentally: the channel cannot change
                                    // during the drain.
                                    let (result, _resume) = outcome.into_parts();
                                    while ct_rx.try_recv().is_ok() {}
                                    match result {
                                        Ok(Some(text)) => {
                                            // Write the composed text back into the
                                            // input bar for review; it is only sent
                                            // to the model when the user presses
                                            // Enter. Reset history navigation so the
                                            // fresh text isn't treated as a history
                                            // position.
                                            app.input = text;
                                            app.reset_cursor_to_end();
                                            app.clear_completion();
                                            app.history_cursor = None;
                                            live_area.ensure_input_cursor_visible(
                                                app.input_cursor,
                                                &app.input,
                                                app.width,
                                            );
                                        }
                                        Ok(None) => {
                                            // Editor aborted without changes; leave
                                            // the input bar exactly as it was.
                                        }
                                        Err(e) => {
                                            // The editor never returned composed
                                            // text; preserve what the user already
                                            // typed in the input bar.
                                            live_area.push_transcript_line(format!(
                                                "editor error: {e}"
                                            ));
                                        }
                                    }
                                    // `_resume` drops here and wakes the parked
                                    // reader after the drain completes.
                                    drop(_resume);
                                }
                                Err(e) => {
                                    // Failed to suspend the input reader (it
                                    // had already exited). No terminal state
                                    // was touched, so there is nothing to
                                    // drain or resume.
                                    live_area.push_transcript_line(format!(
                                        "editor error: {e}"
                                    ));
                                }
                            }
                        }

                        KeyCode::Esc => {
                            // Blocking overlays (single-watch, debug) dismiss first.
                            if app.dismiss_blocking_overlay() {
                                live_area.pop_overlay();
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
                                live_area.pop_overlay();
                                continue;
                            }

                            app.input.clear();
                            app.input_cursor = 0;
                            app.clear_completion();
                            live_area.reset_input_scroll();
                            // If a chat request is in-flight, cancel it on the
                            // daemon. Do not abort `chat_task` — it may be
                            // inside `send_request`, and aborting mid-write
                            // would desync the response FIFO. The task finishes
                            // on its own once the ack arrives.
                            if app.active_request {
                                turn_gen += 1;
                                let cancel_turn = active_turn.clone();
                                let c = Arc::clone(&client);
                                tokio::spawn(async move {
                                    let mut req = serde_json::json!({"op": "cancel_chat"});
                                    if let Some(t) = cancel_turn {
                                        req["turn"] = serde_json::Value::String(t);
                                    }
                                    let _ = c.send_request(req).await;
                                });
                                // Discard the current turn without
                                // committing to history.
                                live_area.clear_current_turn();
                                app.active_request = false;
                                active_turn = None;
                            }
                        }
                        KeyCode::Tab => {
                            // Debug overlay owns a prompt; completion is for the
                            // main input bar only.
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }

                            if let Some(state) = app.completion.as_ref() {
                                let span = state.span.clone();
                                // The initial Tab only inserted the common
                                // prefix, so it must not cycle yet. On
                                // subsequent presses, advance to the next
                                // candidate before committing so the input
                                // always matches the state's current index.
                                if state.contains_current(&app.input) {
                                    app.completion_advance();
                                }
                                let next = app
                                    .completion
                                    .as_ref()
                                    .expect("completion is active")
                                    .current()
                                    .to_string();
                                app.replace_input_span(span.clone(), next.clone());
                                app.update_completion_span(
                                    span.start..span.start + next.len(),
                                );
                            } else {
                                let cursor = app.input_cursor.min(app.input.len());
                                let Some(mut state) = completion::complete(&app.input, cursor) else {
                                    continue;
                                };
                                let span = state.span.clone();
                                if state.candidates.len() == 1 {
                                    let candidate = state.candidates[0].clone();
                                    app.replace_input_span(span, candidate);
                                } else {
                                    let prefix = completion::common_prefix(&state.candidates);
                                    state.index = 0;
                                    state.span = span.start..span.start + prefix.len();
                                    app.set_completion(state);
                                    app.replace_input_span(span, prefix);
                                }
                            }
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::BackTab => {
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }
                            let Some(state) = app.completion.as_ref() else {
                                continue;
                            };
                            let span = state.span.clone();
                            // Rewind first, then commit the previous candidate
                            // so the input always matches the state's current
                            // index.
                            app.completion_rewind();
                            let candidate = app
                                .completion
                                .as_ref()
                                .expect("completion is active")
                                .current()
                                .to_string();
                            app.replace_input_span(span.clone(), candidate.clone());
                            app.update_completion_span(span.start..span.start + candidate.len());
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::Enter => {
                            // Shift+Enter inserts a newline into the main input
                            // bar instead of submitting. The debug overlay prompt
                            // is single-line, so Shift+Enter there is a no-op.
                            if key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
                                if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                    continue;
                                }
                                app.insert_char('\n');
                                app.clear_completion();
                                app.history_cursor = None;
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    &app.input,
                                    app.width,
                                );
                                continue;
                            }

                            // Debug overlay owns a prompt: submit into its
                            // scrollback instead of the main input bar.
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                if let Some(cmd_tx) = &debug_cmd_tx {
                                    handle_overlay_prompt_submit(live_area, cmd_tx);
                                } else {
                                    // Consume the prompt so it doesn't linger.
                                    let _ = live_area.take_debug_prompt();
                                    live_area.push_debug_output(
                                        "(session ended)".to_string(),
                                    );
                                }
                                continue;
                            }

                            let input = std::mem::take(&mut app.input);
                            app.input_cursor = 0;
                            app.clear_completion();
                            live_area.reset_input_scroll();
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
                            if app.push_history(input.clone()) {
                                save_history(&app.input_history);
                            }

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
                                            Some(Overlay::Watch) => {
                                                app.overlay = None;
                                                live_area.pop_overlay();
                                            }
                                            _ => {
                                                app.overlay = Some(Overlay::Watch);
                                                live_area.pop_overlay();
                                                live_area.push_overlay(Box::new(WatchTableWidget::new()));
                                            }
                                        }
                                    }
                                    CommandResult::WatchSingle(id) => {
                                        // Close any existing overlay log follow first.
                                        abort_overlay_log_follow(&mut overlay_log_handle);
                                        overlay_log_following = Some(id.clone());
                                        app.open_watch_single();
                                        live_area.pop_overlay();
                                        live_area.push_overlay(Box::new(WatchSingleWidget::new(id.clone())));
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
                                        app.open_debug();
                                        live_area.pop_overlay();
                                        live_area.push_overlay(Box::new(DebugWidget::new(id.clone())));
                                        // Open a fresh socket and spawn the reader.
                                        match start_debug_session(id.clone()).await {
                                            Ok((cmd_tx, evt_rx, handle)) => {
                                                debug_cmd_tx = Some(cmd_tx);
                                                debug_evt_rx = Some(evt_rx);
                                                debug_task = Some(handle);
                                            }
                                            Err(e) => {
                                                live_area.push_debug_output(format!("error: {e}"));
                                            }
                                        }
                                    }
                                    CommandResult::Lines(lines) => {
                                        let mut block = Vec::with_capacity(lines.len() + 1);
                                        block.push(format!("> {input}"));
                                        block.extend(lines.iter().cloned());
                                        live_area.push_transcript_block(block);
                                    }
                                    CommandResult::RestartChat => {
                                        // Clear current turn.
                                        live_area.clear_current_turn();
                                        // Clear the transcript/scrollback so the
                                        // UI no longer shows the old conversation
                                        // after the daemon has discarded it.
                                        live_area.clear_transcript();
                                        // Clear the live reasoning/tool widgets.
                                        live_area.clear_live_widgets();
                                        live_area.push_styled_line(motif::marble_line());
                                        // Cancel any in-flight turn on the daemon
                                        // (do not abort `chat_task` mid-write).
                                        if app.active_request {
                                            turn_gen += 1;
                                            let cancel_turn = active_turn.clone();
                                            let c = Arc::clone(&client);
                                            tokio::spawn(async move {
                                                let mut req = serde_json::json!({"op": "cancel_chat"});
                                                if let Some(t) = cancel_turn {
                                                    req["turn"] = serde_json::Value::String(t);
                                                }
                                                let _ = c.send_request(req).await;
                                            });
                                        }
                                        // Destroy the current chat gremlin on the
                                        // daemon so the next message starts fresh.
                                        if let Some(id) = app.chat_id.take() {
                                            cleared_chat_ids.insert(id.clone());
                                            let c = Arc::clone(&client);
                                            let tx = result_tx.clone();
                                            tokio::spawn(async move {
                                                let lines = match c
                                                    .send_request(serde_json::json!({
                                                        "op": "stop",
                                                        "id": id,
                                                    }))
                                                    .await
                                                {
                                                    Ok(resp) => format_socket_response("stop", &resp),
                                                    Err(e) => vec![format!("error: {e}")],
                                                };
                                                let _ = tx.send(lines);
                                            });
                                        }
                                        app.input.clear();
                                        app.input_cursor = 0;
                                        app.clear_completion();
                                        live_area.reset_input_scroll();
                                        app.active_request = false;
                                        active_turn = None;
                                    }
                                    CommandResult::Quit => {
                                        break;
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
                                    live_area.push_transcript_line(msg.to_string());
                                    continue;
                                }

                                send_chat_input(
                                    live_area,
                                    &mut app,
                                    &client,
                                    input,
                                    &mut turn_gen,
                                    &mut active_turn,
                                )?;
                            }
                        }
                        KeyCode::Left => {
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }
                            app.cursor_left();
                            app.clear_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::Right => {
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }
                            app.cursor_right();
                            app.clear_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::Home => {
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }
                            app.cursor_home();
                            app.clear_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::End => {
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }
                            app.cursor_end();
                            app.clear_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::Backspace => {
                            if let Some(prompt) = live_area.debug_prompt_mut() {
                                prompt.pop();
                            } else {
                                app.delete_before_cursor();
                                app.clear_completion();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    &app.input,
                                    app.width,
                                );
                            }
                        }
                        KeyCode::Delete => {
                            if app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                continue;
                            }
                            app.delete_at_cursor();
                            app.clear_completion();
                            live_area.ensure_input_cursor_visible(
                                app.input_cursor,
                                &app.input,
                                app.width,
                            );
                        }
                        KeyCode::Char(ch) => {
                            if let Some(prompt) = live_area.debug_prompt_mut() {
                                prompt.push(ch);
                            } else {
                                app.insert_char(ch);
                                app.clear_completion();
                                // Typing a fresh character resets history navigation.
                                app.history_cursor = None;
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    &app.input,
                                    app.width,
                                );
                            }
                        }
                        KeyCode::Up => {
                            // History navigation only applies to the main input bar.
                            if !app.overlay.as_ref().is_some_and(|o| o.has_prompt()) {
                                if let Some(entry) = app.history_up().map(|s| s.to_string()) {
                                    app.input = entry;
                                    app.reset_cursor_to_end();
                                    app.clear_completion();
                                    live_area.ensure_input_cursor_visible(
                                        app.input_cursor,
                                        &app.input,
                                        app.width,
                                    );
                                }
                            }
                        }
                        KeyCode::Down if !app.overlay.as_ref().is_some_and(|o| o.has_prompt())
                            // Only replace the buffer when navigating; when the
                            // cursor is already `None` (fresh line), leave it.
                            && app.history_cursor.is_some() => {
                                app.input = app.history_down().unwrap_or("").to_string();
                                app.reset_cursor_to_end();
                                app.clear_completion();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    &app.input,
                                    app.width,
                                );
                            }
                        _ => {}
                    },
                    Event::Resize(cols, rows) => {
                        live_area.resize(cols, rows)?;
                    }
                    _ => {}
                }
            }

            // ── Daemon events ─────────────────────────────────
            Some(event) = event_rx.recv() => {
                match event {
                    gremlins::executor::DaemonEvent::RunStarted { id, definition, stage } => {
                        log::info!("[run started] {id} ({definition}) stage={stage}");
                        app.on_run_started(id, definition, stage);
                    }
                    gremlins::executor::DaemonEvent::RunCompleted { id, exit_code } => {
                        log::info!("[run completed] {id} (exit {exit_code})");
                        app.on_run_completed(id);
                    }
                    gremlins::executor::DaemonEvent::RunFailed { id, exit_code, error } => {
                        let err_detail = error.as_deref().unwrap_or("");
                        if err_detail.is_empty() {
                            log::info!("[run failed] {id} (exit {exit_code})");
                        } else {
                            log::info!("[run failed] {id} (exit {exit_code}): {err_detail}");
                        }
                        app.on_run_failed(id);
                    }
                    gremlins::executor::DaemonEvent::RunStopped { id } => {
                        log::info!("[run stopped] {id}");
                        app.on_run_stopped(id);
                    }
                    gremlins::executor::DaemonEvent::StageTransition { id, stage } => {
                        log::info!("[{id}] stage → {stage}");
                        app.on_stage_transition(id, stage);
                    }
                    gremlins::executor::DaemonEvent::LogLine { id, line } => {
                        if app.following_log.as_deref() == Some(&id) {
                            live_area.push_transcript_line(line.clone());
                        }
                        if overlay_log_following.as_deref() == Some(&id) {
                            live_area.push_watch_single_line(line);
                        }
                    }
                    gremlins::executor::DaemonEvent::Bail { id, reason } => {
                        log::info!("[{id}] bail: {reason}");
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
                    live_area.push_transcript_line(full);
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
                                live_area.push_transcript_line(line.to_string());
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
                                live_area.push_watch_single_line(line.to_string());
                            }
                        }
                    }
                    Some("overlay_error") => {
                        if let (Some(id), Some(message)) = (
                            raw.get("id").and_then(|v| v.as_str()),
                            raw.get("message").and_then(|v| v.as_str()),
                        ) {
                            if overlay_log_following.as_deref() == Some(id) {
                                live_area.push_watch_single_line(format!("error: {message}"));
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
                            live_area.push_transcript_line(full.clone());
                            app.following_log = None;
                        }
                    }
                    _ => {}
                }
            }

            // ── Socket-op results ─────────────────────────────
            Some(lines) = result_rx.recv() => {
                live_area.push_transcript_block(lines);
            }

            // ── Chat events ────────────────────────────────
            Some(event) = chat_rx.recv() => {
                // Drop events that belong to a superseded turn (a turn that
                // was cancelled by Esc or replaced by a newer submit).
                let event_turn = match &event {
                    ChatEvent::StreamChunk { turn, .. }
                    | ChatEvent::ReasoningChunk { turn, .. }
                    | ChatEvent::ToolResult { turn, .. }
                    | ChatEvent::TurnComplete { turn, .. }
                    | ChatEvent::Done { turn, .. }
                    | ChatEvent::Ended { turn, .. }
                    | ChatEvent::Error { turn, .. }
                    | ChatEvent::ChatId { turn, .. } => turn.clone(),
                };
                if let (Some(active), Some(ev)) = (&active_turn, &event_turn) {
                    if active != ev {
                        log::debug!("dropping stale chat event for turn {ev} (active {active})");
                        continue;
                    }
                }

                match event {
                    ChatEvent::StreamChunk { text, .. } => {
                        log::debug!("StreamChunk len={}", text.len());
                        live_area.push_response(&text);
                    }
                    ChatEvent::ReasoningChunk { text, .. } => {
                        log::debug!("ReasoningChunk len={}", text.len());
                        live_area.push_reasoning_text(&text);
                    }
                    ChatEvent::ToolResult { name, output, .. } => {
                        // The bare tool result is intentionally not rendered;
                        // `ChatEvent::TurnComplete` is the sole source of
                        // tool-event lines, enriched with call arguments.
                        // The event is still matched (and logged) because
                        // other consumers rely on its payload.
                        log::debug!("ToolResult name={name} output_len={}", output.len());
                    }
                    ChatEvent::TurnComplete { text, tool_calls, .. } => {
                        log::debug!("TurnComplete text_len={} tool_calls={:?}", text.len(), tool_calls);
                        // `text` is the model's full response for the just-completed
                        // turn — the same text streamed via StreamChunk. Reconcile
                        // `response_text` so the display shows the complete response
                        // even when broadcast StreamChunk events were dropped due to
                        // channel lag. `response_text` accumulates across the agent
                        // loop, so only the current turn's trailing slice is repaired.
                        live_area.reconcile_response(&text);
                        // Record tool calls so they appear in the transcript rather
                        // than being silently lost. Read/Edit calls are filtered per
                        // DESIGN.md § 8.2.1: they're noisy file-access tools whose
                        // output belongs in the transcript, not the live tool tail.
                        for tc in &tool_calls {
                            if tc.starts_with("Read") || tc.starts_with("Edit") {
                                continue;
                            }
                            live_area.push_tool(tc);
                        }
                    }
                    ChatEvent::Done { text, summary, usage, id, .. } => {
                        log::debug!("Done text_len={} summary_len={} has_current_turn={}",
                            text.len(), summary.len(), live_area.has_current_turn());
                        // Capture the gremlin id on the first turn so
                        // subsequent messages target the same chat gremlin.
                        // Do this before the has_current_turn skip so a failed
                        // first turn still records the id for the next message.
                        if let Some(id) = id {
                            if !cleared_chat_ids.contains(&id) {
                                app.chat_id = Some(id);
                            }
                        }
                        if !live_area.has_current_turn() {
                            log::debug!("Done SKIPPED — no current turn");
                            continue;
                        }
                        // The Done text is the Done-tool summary — gremlin
                        // machinery. Only use it when the model produced no
                        // streamed text (e.g. called Done without saying
                        // anything). Otherwise the StreamChunk text IS the
                        // response. The summary is appended to the response
                        // (unless empty or already equal to the text on
                        // screen) so it is displayed as part of the turn.
                        live_area.finish_response(&text, &summary);

                        // Unpin the turn and clear its draft state so the
                        // finished response is committed to the transcript.
                        live_area.finish_turn();

                        // Build usage summary line.
                        if let Some(usage) = usage {
                            if let (Some(prompt), Some(completion)) = (
                                usage.get("input_tokens").and_then(|v| v.as_u64()),
                                usage.get("output_tokens").and_then(|v| v.as_u64()),
                            ) {
                                let cache_read = usage
                                    .get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let pct = if prompt > 0 {
                                    format!("{:.0}% cached",
                                        (cache_read as f64 / prompt as f64) * 100.0)
                                } else {
                                    String::new()
                                };
                                let summary = format!(
                                    "{} tokens · {pct}",
                                    prompt + completion
                                );
                                live_area.push_transcript_line(summary);
                            }
                        }

                        app.active_request = false;
                        active_turn = None;

                        // Repaint immediately so the finished response text
                        // and Done summary are visible without waiting for the
                        // next input event.
                        live_area.compose(&app)?;
                    }
                    ChatEvent::Ended { reason, id, .. } => {
                        if let Some(id) = id {
                            if !cleared_chat_ids.contains(&id) {
                                app.chat_id = Some(id);
                            }
                        }
                        if !live_area.has_current_turn() {
                            continue;
                        }
                        app.active_request = false;
                        active_turn = None;
                        let msg = format!("chat ended: {reason}");
                        live_area.finish_turn();
                        live_area.push_transcript_line(msg);

                        // Repaint immediately so the ended state is visible
                        // without waiting for the next input event.
                        live_area.compose(&app)?;
                    }
                    ChatEvent::Error { message: msg, id, .. } => {
                        if let Some(id) = id {
                            if !cleared_chat_ids.contains(&id) {
                                app.chat_id = Some(id);
                            }
                        }
                        if !live_area.has_current_turn() {
                            continue;
                        }
                        app.active_request = false;
                        active_turn = None;
                        let full = format!("chat error: {msg}");
                        live_area.finish_turn();
                        live_area.push_transcript_line(full);

                        // Repaint immediately so the error state is visible
                        // without waiting for the next input event.
                        live_area.compose(&app)?;
                    }
                    ChatEvent::ChatId { id, .. } => {
                        // Store the gremlin id immediately from the ack so
                        // Esc + resubmit targets the same chat gremlin rather
                        // than creating a second one.
                        log::debug!("ChatId captured: {id}");
                        if !cleared_chat_ids.contains(&id) {
                            app.chat_id = Some(id);
                        }
                    }
                }
            }

            // ── Debug session events ──────────────────────────
            Some(event) = recv_debug(&mut debug_evt_rx) => {
                match event {
                    DebugEvent::Ready { id } => {
                        live_area.mark_debug_ready();
                        live_area.push_debug_output(format!("connected to gremlin {id}"));
                    }
                    DebugEvent::TurnComplete { text, tool_calls, .. } => {
                        if !text.is_empty() {
                            live_area.push_debug_output(text);
                        }
                        for tc in &tool_calls {
                            live_area.push_debug_output(format!("  [tool: {tc}]"));
                        }
                        live_area.push_debug_output(
                            "debug: turn complete — agent paused".to_string(),
                        );
                    }
                    DebugEvent::Done { text } => {
                        if !text.is_empty() {
                            live_area.push_debug_output(text);
                        }
                        live_area.push_debug_output("debug: agent called Done".to_string());
                        live_area.set_debug_connected(false);
                        debug_task = None;
                        debug_cmd_tx = None;
                        debug_evt_rx = None;
                    }
                    DebugEvent::Ended { reason } => {
                        live_area.push_debug_output(format!("debug: session ended ({reason})"));
                        live_area.set_debug_connected(false);
                        debug_task = None;
                        debug_cmd_tx = None;
                        debug_evt_rx = None;
                    }
                    DebugEvent::StreamChunk { text } => {
                        live_area.push_debug_output(text);
                    }
                    DebugEvent::ReasoningChunk { text } => {
                        live_area.push_debug_output(text);
                    }
                    DebugEvent::ToolResult { name, output } => {
                        live_area.push_debug_output(format!("[tool: {name}] {output}"));
                    }
                    DebugEvent::Error(msg) => {
                        live_area.push_debug_output(format!("debug: error: {msg}"));
                    }
                    DebugEvent::Disconnected => {
                        live_area.push_debug_output("debug: disconnected".to_string());
                        live_area.set_debug_connected(false);
                        debug_task = None;
                        debug_cmd_tx = None;
                        debug_evt_rx = None;
                    }
                }
            }
        }
    }

    // ── Exit ──────────────────────────────────────────────────────

    // The daemon owns the conversation; nothing to commit on exit.
    app.active_request = false;

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
        "artifact" => {
            if let Some(keys) = resp.get("keys").and_then(|v| v.as_array()) {
                if keys.is_empty() {
                    return vec!["(no artifacts)".to_string()];
                }
                keys.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            } else if let Some(content) = resp.get("content").and_then(|v| v.as_str()) {
                vec![content.to_string()]
            } else {
                vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
            }
        }
        "launch" => match resp.get("id").and_then(|v| v.as_str()) {
            Some(id) => vec![id.to_string()],
            None => {
                vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
            }
        },
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

#[cfg(test)]
mod tests {
    use super::build_chat_request;
    use std::collections::HashMap;

    #[test]
    fn chat_request_carries_project_root() {
        let env = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        let req = build_chat_request("push this branch", "3", None, "/home/user/proj", "", &env);
        assert_eq!(req["op"], "chat");
        assert_eq!(req["text"], "push this branch");
        assert_eq!(req["turn"], "3");
        assert_eq!(req["project_root"], "/home/user/proj");
        assert_eq!(req["env"]["PATH"], "/usr/bin");
        assert!(req.get("id").is_none());
    }

    #[test]
    fn chat_request_includes_id_when_present() {
        let env = HashMap::new();
        let req = build_chat_request("hi", "2", Some("chat-0001"), "/p", "", &env);
        assert_eq!(req["id"], "chat-0001");
        assert_eq!(req["project_root"], "/p");
    }

    #[test]
    fn chat_request_prepends_context() {
        let env = HashMap::new();
        let req = build_chat_request("hello", "1", None, "/p", "Current branch: main", &env);
        assert_eq!(req["text"], "Current branch: main\n\nhello");
    }
}
