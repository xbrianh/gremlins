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
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind};
use tokio::io::BufReader;
use tokio::sync::mpsc;

use app::{ActiveRun, App, Screen};
use chat::ChatEvent;
use commands::{dispatch, CommandResult};
use editor::{open_editor, ReaderChannels, SuspendHandle};
use gremlins::executor::socket;
use screen::LiveArea;

/// A daemon `chat_model` lookup result tagged with the session generation it
/// was issued for. Stale generations are ignored by the event loop.
type ChatModelQuery = (u64, Result<Option<String>, String>);

/// Drop-guard that aborts a JoinHandle on drop, ensuring a dedicated
/// log-follow socket is closed even when the outer forwarding task is
/// cancelled via `abort()`.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Abort the dedicated log-follow task for the watch stream pane.
fn abort_watch_follow(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        handle.abort();
    }
}

/// Close the watch stream pane: release the follow socket and clear the
/// rendered log tail.
fn close_watch_follow(
    app: &mut App,
    live_area: &mut LiveArea,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
) {
    abort_watch_follow(watch_follow_handle);
    app.watch_following = None;
    app.watch_follow_error = None;
    live_area.clear_watch_stream();
}

/// Open (or switch) the watch stream pane for `id`.
///
/// At most one watch follow socket exists at a time: any prior follow is
/// aborted before the new one is spawned. Lines are scoped through the
/// existing `view_log` channel so queued lines from a superseded follow are
/// never painted into the new pane.
fn start_watch_follow(
    app: &mut App,
    live_area: &mut LiveArea,
    client: &Arc<client::DaemonClient>,
    id: String,
    log_tx: &mpsc::UnboundedSender<serde_json::Value>,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
    watch_follow_generation: &mut u64,
) {
    abort_watch_follow(watch_follow_handle);
    live_area.clear_watch_stream();
    app.watch_following = Some(id.clone());
    app.watch_follow_error = None;

    // Each follow is tagged with a unique generation so messages queued by
    // an aborted/superseded follow are never accepted after the same id is
    // reopened.
    *watch_follow_generation = watch_follow_generation.wrapping_add(1);
    let generation = *watch_follow_generation;

    let log_tx = log_tx.clone();
    let client = Arc::clone(client);
    let follow_id = id.clone();
    let follow_task = tokio::spawn(async move {
        match client.follow_log(&follow_id).await {
            Ok((mut rx, handle)) => {
                let _guard = AbortOnDrop(handle);
                while let Some(raw) = rx.recv().await {
                    let Some(line) = raw.get("line").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let tagged = serde_json::json!({
                        "type": "view_log",
                        "id": follow_id,
                        "generation": generation,
                        "line": line,
                    });
                    if log_tx.send(tagged).is_err() {
                        break;
                    }
                }
                // The socket closed without an explicit run-terminal event.
                // Surface that state instead of leaving the pane claiming it
                // is actively following.
                let _ = log_tx.send(serde_json::json!({
                    "type": "view_error",
                    "id": follow_id,
                    "generation": generation,
                    "message": "log stream closed",
                }));
            }
            Err(e) => {
                let _ = log_tx.send(serde_json::json!({
                    "type": "view_error",
                    "id": follow_id,
                    "generation": generation,
                    "message": e,
                }));
            }
        }
    });
    *watch_follow_handle = Some(follow_task);
}

/// Ctrl+C is treated as a reset key (exactly like Esc). A second press
/// within this window quits the TUI; a later press becomes a fresh first
/// press. `/quit` and Ctrl+D on empty input remain the explicit exits.
const PENDING_QUIT_WINDOW: Duration = Duration::from_secs(1);

/// Classify a Ctrl+C press at `now` against the pending-quit state.
///
/// Returns `true` when a prior first press is still within
/// `PENDING_QUIT_WINDOW`, in which case this press should quit the TUI.
/// Otherwise the press becomes the new first press — (re-)arming the
/// confirmation window — and `false` is returned.
fn ctrl_c_should_quit(pending_quit: &mut Option<Instant>, now: Instant) -> bool {
    let quit = pending_quit.is_some_and(|first| now.duration_since(first) <= PENDING_QUIT_WINDOW);
    if !quit {
        *pending_quit = Some(now);
    }
    quit
}

/// Exit any non-chat screen and tear down its session-owned aux sockets.
///
/// Returns `true` when a full-screen view was active and is now dropped.
#[allow(clippy::too_many_arguments)]
fn exit_current_screen(
    app: &mut App,
    live_area: &mut LiveArea,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
    debug_task: &mut Option<tokio::task::JoinHandle<()>>,
    debug_cmd_tx: &mut Option<mpsc::UnboundedSender<serde_json::Value>>,
    debug_evt_rx: &mut Option<mpsc::UnboundedReceiver<DebugEvent>>,
) -> bool {
    let screen = app.screen.clone();
    if !app.exit_screen() {
        return false;
    }

    live_area.exit_screen_view();

    match screen {
        Screen::Debug { .. } => {
            // Drop the debug socket so the daemon sees EOF.
            if let Some(handle) = debug_task.take() {
                handle.abort();
            }
            *debug_cmd_tx = None;
            *debug_evt_rx = None;
        }
        Screen::Runs | Screen::Chat => {
            close_watch_follow(app, live_area, watch_follow_handle);
        }
    }

    true
}

/// Run the shared Esc / first-Ctrl+C reset path.
///
/// Ordering is deliberate: an open watch stream pane closes first, then any
/// full-screen view returns to chat, then the main input bar is cleared and
/// any in-flight chat turn is cancelled.
///
/// Returns `true` when a screen was exited or a watch pane was closed, in
/// which case the caller should `continue` the event loop immediately; the
/// clear/cancel path returns `false` and falls through, exactly as the
/// original Esc arm did.
#[allow(clippy::too_many_arguments)]
fn handle_esc_reset(
    app: &mut App,
    live_area: &mut LiveArea,
    watch_follow_handle: &mut Option<tokio::task::JoinHandle<()>>,
    debug_task: &mut Option<tokio::task::JoinHandle<()>>,
    debug_cmd_tx: &mut Option<mpsc::UnboundedSender<serde_json::Value>>,
    debug_evt_rx: &mut Option<mpsc::UnboundedReceiver<DebugEvent>>,
    turn_gen: &mut u64,
    active_turn: &mut Option<String>,
    client: &Arc<client::DaemonClient>,
) -> bool {
    // Note: Esc only clears the ephemeral tool slot when it actually cancels
    // an in-flight turn (via `clear_current_turn` below). Clearing it up
    // front was wrong: Esc on the Runs screen (closing a pane or returning
    // to chat) while a turn streams in the background must not wipe that
    // turn's live tool streams.

    // On the watch screen, Esc first closes an open stream pane and only
    // then exits back to chat.
    if app.screen == Screen::Runs && app.watch_following.is_some() {
        close_watch_follow(app, live_area, watch_follow_handle);
        return true;
    }

    if exit_current_screen(
        app,
        live_area,
        watch_follow_handle,
        debug_task,
        debug_cmd_tx,
        debug_evt_rx,
    ) {
        return true;
    }

    app.input.clear();
    app.input_cursor = 0;
    app.clear_completion();
    live_area.reset_input_scroll();
    // If a chat request is in-flight, cancel it on the daemon. Do not abort
    // `chat_task` — it may be inside `send_request`, and aborting mid-write
    // would desync the response FIFO. The task finishes on its own once the
    // ack arrives.
    if app.active_request {
        *turn_gen += 1;
        let cancel_turn = active_turn.clone();
        let c = Arc::clone(client);
        tokio::spawn(async move {
            let mut req = serde_json::json!({"op": "cancel_chat"});
            if let Some(t) = cancel_turn {
                req["turn"] = serde_json::Value::String(t);
            }
            let _ = c.send_session_request(req).await;
        });
        // Discard the current turn without committing to history.
        live_area.clear_current_turn();
        app.active_request = false;
        *active_turn = None;
    }

    false
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
/// the caller (see [`send_chat_input`]). It is carried as a separate request
/// field so the daemon can append it to the chat system prompt as low-trust
/// metadata rather than folding it into the operator's message.
fn build_chat_request(
    text: &str,
    turn: &str,
    project_root: &str,
    context: &str,
    env: &HashMap<String, String>,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "op": "chat",
        "text": text,
        "turn": turn,
        "project_root": project_root,
        "env": env,
    });
    if !context.is_empty() {
        request["context"] = serde_json::Value::String(context.to_string());
    }
    request
}

/// Submit plain-text chat input to the daemon.
///
/// This is the shared tail of the Enter and `Ctrl+G`-editor paths. Caller
/// is responsible for taking the input, recording history, and stopping any
/// log follow first. The caller must have already checked `active_request`
/// and `session_recreating`.
#[allow(clippy::too_many_arguments)]
fn send_chat_input(
    live_area: &mut LiveArea,
    app: &mut App,
    client: &Arc<client::DaemonClient>,
    input: String,
    turn_gen: &mut u64,
    active_turn: &mut Option<String>,
    session_generation: u64,
    model_tx: mpsc::UnboundedSender<ChatModelQuery>,
) -> io::Result<()> {
    if app.active_request {
        let msg = "a request is already in progress — wait for the response";
        live_area.push_transcript_line(msg.to_string());
        return Ok(());
    }

    // Start a new turn with the prompt.
    live_area.clear_tool_output();
    live_area.submit_prompt(input.clone());

    // Force an immediate frame so the pinned prompt appears without waiting
    // for the first daemon event.
    live_area.compose(app)?;

    app.active_request = true;
    *turn_gen += 1;
    let turn_id = turn_gen.to_string();
    *active_turn = Some(turn_id.clone());
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
            &project_root.to_string_lossy(),
            &context,
            &env,
        );
        match client_for_chat
            .send_session_request_for_generation(request, session_generation)
            .await
        {
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
                        turn: Some(turn_id.clone()),
                    });
                } else {
                    // The chat turn is now airborne: the daemon has created
                    // (or refreshed) the session's chat gremlin and patched
                    // its `state.json` model. Refresh the info-bar cache from
                    // the daemon, which owns model state. Spawn rather than
                    // await so this task finishes as soon as the ack is
                    // handled and never borrows the client longer than needed.
                    let client_for_model = Arc::clone(&client_for_chat);
                    let model_tx = model_tx.clone();
                    tokio::spawn(async move {
                        let result = query_chat_model(&client_for_model, session_generation).await;
                        let _ = model_tx.send(result);
                    });
                }
            }
            Err(e) => {
                let _ = chat_tx.send(ChatEvent::Error {
                    message: e,
                    turn: Some(turn_id.clone()),
                });
            }
        }
    });

    Ok(())
}

/// Query the daemon for the active chat gremlin's model and return the
/// result tagged with the session generation the query was issued for.
///
/// The response has `model` as a string or `null` when the session has no
/// chat gremlin yet. A non-`ok` reply, a missing `model` key, or a
/// non-string/non-null model is treated as an error so a malformed reply can
/// never silently clear the info bar. The TUI never reads `tui.json` or
/// `state.json`; the daemon owns model state.
async fn query_chat_model(
    client: &Arc<client::DaemonClient>,
    session_generation: u64,
) -> ChatModelQuery {
    let result = client
        .send_session_request_for_generation(
            serde_json::json!({ "op": "chat_model" }),
            session_generation,
        )
        .await
        .and_then(|resp| {
            if resp.get("type").and_then(|v| v.as_str()) != Some("ok") {
                return Err(resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("chat_model request failed")
                    .to_string());
            }

            match resp.get("model") {
                Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::String(model)) => Ok(Some(model.clone())),
                _ => Err("chat_model response missing 'model' field".to_string()),
            }
        });

    (session_generation, result)
}

/// Route a submitted line from the full-screen debug view prompt.
///
/// The debug socket is opened when `/debug <id>` is dispatched; this
/// function only forwards subsequent prompt submissions to the daemon
/// over the already-established session.
fn handle_debug_prompt_submit(
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

    // Echo the command in the debug view scrollback.
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
    ToolOutputChunk {
        id: String,
        name: String,
        output: String,
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
        Some("tool_output_chunk") => DebugEvent::ToolOutputChunk {
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
            output: value
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

/// Read a canonical token/cache field from a usage object.
fn usage_field(usage: Option<&serde_json::Value>, field: &str) -> Option<u64> {
    usage.and_then(|v| v.get(field)).and_then(|v| v.as_u64())
}

/// Placeholder reasoning title shown as soon as the first reasoning chunk of
/// a turn arrives, and the fixed label prefix of the live stats title. Shared
/// by [`stats_title`] here and the reasoning band in `screen.rs` so the
/// placeholder→stats handoff can never drift between two hard-coded strings.
pub(crate) const REASONING_PLACEHOLDER_TITLE: &str = "clanking";

/// Format one usage object as the compact token/cache portion of the live
/// stats title. Missing values render as dashes. The cache-hit rate is
/// grouped inside the `in` field as a percentage of prompt tokens.
fn usage_title_part(usage: Option<&serde_json::Value>) -> String {
    let field = |name: &str| usage_field(usage, name);
    let num = |name: &str| match field(name) {
        Some(v) => v.to_string(),
        None => "-".to_string(),
    };

    let prompt = field("prompt_tokens");
    let in_part = match prompt {
        Some(p) if p > 0 => {
            let cached = field("cached_input_tokens");
            let pct = match cached {
                Some(c) => format!("{:.0}%", (c as f64 / p as f64) * 100.0),
                None => "-".to_string(),
            };
            format!(
                "in {p} (cache {pct}, +{} new)",
                num("cache_creation_input_tokens")
            )
        }
        Some(_) => format!(
            "in 0 (cache -, +{} new)",
            num("cache_creation_input_tokens")
        ),
        None => "in -".to_string(),
    };

    format!(
        "{in_part} · out {} · reason {}",
        num("completion_tokens"),
        num("reasoning_tokens")
    )
}

/// Format the conversation-wide Σ (total) portion of the stats title.
/// Only completion and reasoning tokens are summed. Prompt-token totals are
/// not monotonic across conversation compaction, so they are not summed; the
/// per-turn prompt size is already shown in the `in` field.
fn totals_title_part(total: Option<&serde_json::Value>) -> String {
    let num = |name: &str| match usage_field(total, name) {
        Some(v) => v.to_string(),
        None => "-".to_string(),
    };

    format!(
        "out {} · reason {}",
        num("completion_tokens"),
        num("reasoning_tokens")
    )
}

/// Build the reasoning-stream stats title shown on each `TurnComplete`:
/// the `clanking` header label, the just-completed turn's delta, and the
/// conversation-wide completion/reasoning total.
fn stats_title(stats: Option<&serde_json::Value>, total: Option<&serde_json::Value>) -> String {
    let turn = stats
        .and_then(|v| v.get("turn"))
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".to_string());
    let parts = [
        format!("{REASONING_PLACEHOLDER_TITLE} · turn {turn}"),
        usage_title_part(stats),
        format!("Σ {}", totals_title_part(total)),
    ];
    parts.join(" · ")
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
    client: &Arc<client::DaemonClient>,
    id: String,
) -> Result<
    (
        mpsc::UnboundedSender<serde_json::Value>,
        mpsc::UnboundedReceiver<DebugEvent>,
        tokio::task::JoinHandle<()>,
    ),
    String,
> {
    let stream = client.open_aux_stream().await?;
    let (read_half, mut write_half) = stream.into_split();

    let request =
        serde_json::json!({ "op": "debug", "id": id, "session_id": client.session_id().await });
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

/// Fatal TUI error, classified by whether it has already been reported on
/// the operator's terminal.
#[derive(Debug)]
pub enum RunError {
    /// The error was written to stdout after the terminal was restored. The
    /// caller must still exit nonzero, but must not print the message again.
    Reported,
    /// The error occurred while stderr was still the operator's terminal.
    /// The caller should print `gremlins: <message>` to stderr and exit
    /// nonzero.
    Unreported(String),
}

/// Initialise the terminal, run the event loop, and restore on exit.
pub async fn run() -> Result<(), RunError> {
    // Refuse to start the TUI when stdout is not a terminal.
    if !std::io::stdout().is_terminal() {
        eprintln!("gremlins: stdout is not a terminal — use `gremlins ls` to list gremlins");
        return Ok(());
    }

    // Load the process-global config before taking over the terminal or
    // redirecting stderr. A malformed settings.yaml is therefore returned
    // to the CLI entry point while stderr is still the operator's terminal.
    gremlins::config::init_global().map_err(|e| RunError::Unreported(e.to_string()))?;

    // Terminal setup also runs before the stderr redirect so setup failures
    // land on the real stderr. `LiveArea::new` leaves raw mode entered, so
    // on exit the `LiveArea` drop guard restores the cursor and raw mode.
    let mut live_area =
        LiveArea::new().map_err(|e| RunError::Unreported(format!("terminal setup failed: {e}")))?;

    // Snapshot the real stderr before redirecting it, so a post-redirect
    // failure can restore it before reporting the error. Use a close-on-exec
    // duplicate so a daemon spawned by `run_app` does not inherit a live
    // copy of the operator's stderr (which would keep a pipe reader waiting
    // for EOF after the TUI exits).
    let saved_stderr = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 0) };

    // Redirect stderr to tui.log so debug logs persist next to executor.log.
    // Config is loaded first so a custom `paths.state-root` is honored for
    // the log path. Track whether the redirect succeeded: if it did not,
    // stderr is still the operator's terminal and the CLI entry point can
    // report post-loop failures there as usual.
    let mut stderr_redirected = false;
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
        stderr_redirected = true;
    }

    if let Err(e) = run_app(&mut live_area).await {
        // Restore the terminal first so the message is printed on a sane
        // terminal rather than into raw mode, then restore the saved stderr
        // (when we were able to snapshot it).
        drop(live_area);
        if saved_stderr >= 0 {
            unsafe { libc::dup2(saved_stderr, 2) };
            unsafe { libc::close(saved_stderr) };
        }

        if stderr_redirected {
            // The operator's stderr may itself have been redirected before
            // launch (for example `gremlins 2>/dev/null`), so restoring fd 2
            // is not enough to make the error visible. Issue #1951 requires
            // these fatal event-loop/daemon errors on stdout after the
            // terminal is restored. The CLI exits nonzero without printing
            // this message again.
            println!("gremlins: {e}");
            return Err(RunError::Reported);
        }

        return Err(RunError::Unreported(e.to_string()));
    }

    if saved_stderr >= 0 {
        unsafe { libc::close(saved_stderr) };
    }
    Ok(())
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
    match client
        .send_control_request(serde_json::json!({"op": "ls"}))
        .await
    {
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

    // Query the daemon for the active chat model before the first prompt so
    // the info bar can show a restored session's model immediately.
    let session_generation = client.session_generation().await;
    match query_chat_model(&client, session_generation).await {
        (_, Ok(model)) => app.chat_model = model,
        (_, Err(e)) => log::debug!("initial chat_model query failed: {e}"),
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

    // ── Channel for session-recreation completion ─────────────────
    let (reset_tx, mut reset_rx) = mpsc::unbounded_channel::<Result<(), String>>();

    // ── Channel for daemon chat-model lookups ─────────────────────
    let (model_tx, mut model_rx) = mpsc::unbounded_channel::<ChatModelQuery>();

    // ── Channel for run-snapshot refreshes (after event_lagged) ───
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel::<HashMap<String, ActiveRun>>();

    // ── Log follow state ──────────────────────────────────────────
    let mut log_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Dedicated follow for the inline watch stream pane.
    let mut watch_follow_handle: Option<tokio::task::JoinHandle<()>> = None;
    // Monotonic token for the current watch follow; queued lines must match
    // both id and generation before being painted.
    let mut watch_follow_generation: u64 = 0;
    // ── Chat turn state ────────────────────────────────────────
    // (the chat request is sent from a detached task; we no longer need to
    // abort it, so no JoinHandle is kept)
    // Monotonic turn id, bumped on every submit and every Esc so that a
    // stale turn's terminal events (which arrive after a cancel or a new
    // submit) can be recognised and dropped.
    let mut turn_gen: u64 = 0;
    // The turn id of the currently active turn, if any.
    let mut active_turn: Option<String> = None;
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // ── Debug session state ────────────────────────────────────────
    // Command sender for the active debug socket (talk/quit/bail/continue).
    let mut debug_cmd_tx: Option<mpsc::UnboundedSender<serde_json::Value>> = None;
    // Incoming debug events from the socket reader task.
    let mut debug_evt_rx: Option<mpsc::UnboundedReceiver<DebugEvent>> = None;
    // Handle for the debug socket reader task (aborted on Esc).
    let mut debug_task: Option<tokio::task::JoinHandle<()>> = None;

    // ── Startup banner ────────────────────────────────────────────
    for line in motif::banner() {
        live_area.push_styled_line(line);
    }

    // ── Ctrl+C pending-quit state (event-loop-local input routing) ──
    let mut pending_quit: Option<Instant> = None;

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
                            let now = Instant::now();
                            if ctrl_c_should_quit(&mut pending_quit, now) {
                                break;
                            }
                            // First press (or an expired previous press): reset
                            // exactly like Esc. The one-second quit window is
                            // already armed by `ctrl_c_should_quit`.
                            if handle_esc_reset(
                                &mut app,
                                live_area,
                                &mut watch_follow_handle,
                                &mut debug_task,
                                &mut debug_cmd_tx,
                                &mut debug_evt_rx,
                                &mut turn_gen,
                                &mut active_turn,
                                &client,
                            ) {
                                continue;
                            }
                        }
                        KeyCode::Char('d')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            // Ctrl+D only dismisses the debug view's prompt;
                            // Runs falls through to the empty-input quit path
                            // below, preserving the documented quit key.
                            if app.screen.has_prompt()
                                && exit_current_screen(
                                    &mut app,
                                    live_area,
                                    &mut watch_follow_handle,
                                    &mut debug_task,
                                    &mut debug_cmd_tx,
                                    &mut debug_evt_rx,
                                )
                            {
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
                        KeyCode::Char('o')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            if app.screen != Screen::Runs {
                                continue;
                            }
                            let Some(selected) = app.watch_selection.clone() else {
                                continue;
                            };
                            if app.watch_following.as_deref() == Some(selected.as_str()) {
                                close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                            } else {
                                start_watch_follow(
                                    &mut app,
                                    live_area,
                                    &client,
                                    selected,
                                    &log_tx,
                                    &mut watch_follow_handle,
                                    &mut watch_follow_generation,
                                );
                            }
                        }
                        KeyCode::Char('g')
                            if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            if !app.screen.is_chat() {
                                continue;
                            }
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
                            if handle_esc_reset(
                                &mut app,
                                live_area,
                                &mut watch_follow_handle,
                                &mut debug_task,
                                &mut debug_cmd_tx,
                                &mut debug_evt_rx,
                                &mut turn_gen,
                                &mut active_turn,
                                &client,
                            ) {
                                continue;
                            }
                        }
                        KeyCode::Tab => {
                            // Debug view owns a prompt; completion is for the
                            // main input bar only.
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            // Debug view owns a prompt: submit into its
                            // scrollback instead of the main input bar.
                            if app.screen.has_prompt() {
                                if let Some(cmd_tx) = &debug_cmd_tx {
                                    handle_debug_prompt_submit(live_area, cmd_tx);
                                } else {
                                    // Consume the prompt so it doesn't linger.
                                    let _ = live_area.take_debug_prompt();
                                    live_area.push_debug_output(
                                        "(session ended)".to_string(),
                                    );
                                }
                                continue;
                            }

                            // Any other non-chat screen has no input bar; ignore
                            // Enter until the operator returns to chat.
                            if !app.screen.is_chat() {
                                continue;
                            }

                            let input = std::mem::take(&mut app.input);
                            app.input_cursor = 0;
                            app.clear_completion();
                            live_area.reset_input_scroll();
                            if input.is_empty() {
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
                                    CommandResult::ShowRuns => {
                                        close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                        app.show_runs();
                                        live_area.show_runs();
                                    }
                                    CommandResult::Debug(id) => {
                                        close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                        app.show_debug(id.clone());
                                        live_area.show_debug(id.clone());
                                        // Open a fresh socket and spawn the reader.
                                        match start_debug_session(&client, id.clone()).await {
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
                                        if app.session_recreating {
                                            live_area.push_transcript_line(
                                                "session reset already in progress".to_string(),
                                            );
                                            continue;
                                        }
                                        // Clear current turn.
                                        live_area.clear_current_turn();
                                        // Clear the transcript/scrollback so the
                                        // UI no longer shows the old conversation
                                        // after the daemon has discarded it. The
                                        // transcript also clears its embedded
                                        // reasoning and tool slots.
                                        live_area.clear_transcript();
                                        for line in motif::banner() {
                                            live_area.push_styled_line(line);
                                        }
                                        // `/clear` closes the current session and
                                        // opens a fresh one. The daemon cancels
                                        // any in-flight turn and drops the old
                                        // chat gremlin when the session closes.
                                        // Drop every socket/stream that belonged
                                        // to the dying session first, before the
                                        // new session is installed.
                                        turn_gen += 1;
                                        active_turn = None;
                                        app.active_request = false;
                                        if let Some(handle) = log_follow_handle.take() {
                                            handle.abort();
                                        }
                                        app.following_log = None;
                                        close_watch_follow(&mut app, live_area, &mut watch_follow_handle);
                                        if let Some(handle) = debug_task.take() {
                                            handle.abort();
                                        }
                                        debug_cmd_tx = None;
                                        debug_evt_rx = None;
                                        live_area.set_debug_connected(false);
                                        // Block new chat submissions and further
                                        // `/clear` requests until the replacement
                                        // session socket is installed. Recreate
                                        // requests are also serialized inside
                                        // `DaemonClient::recreate_session`.
                                        app.session_recreating = true;
                                        let c = Arc::clone(&client);
                                        let tx = reset_tx.clone();
                                        tokio::spawn(async move {
                                            let result = c.recreate_session().await;
                                            let _ = tx.send(result);
                                        });
                                        app.input.clear();
                                        app.input_cursor = 0;
                                        app.clear_completion();
                                        live_area.reset_input_scroll();
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
                                                let client = Arc::clone(&client);
                                                let follow_task = tokio::spawn(async move {
                                                    match client.follow_log(&id).await {
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
                                                let lines = match c.send_control_request(payload).await {
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
                                if app.session_recreating {
                                    live_area.push_transcript_line(
                                        "session reset is in progress — wait before sending"
                                            .to_string(),
                                    );
                                    continue;
                                }
                                if app.active_request {
                                    let msg = "a request is already in progress — wait for the response";
                                    live_area.push_transcript_line(msg.to_string());
                                    continue;
                                }

                                // Capture the session generation at submission
                                // time. If `/clear` recreates the session while
                                // repository context is still being gathered,
                                // the request is discarded atomically before it
                                // can execute in the new conversation.
                                let session_generation = client.session_generation().await;
                                send_chat_input(
                                    live_area,
                                    &mut app,
                                    &client,
                                    input,
                                    &mut turn_gen,
                                    &mut active_turn,
                                    session_generation,
                                    model_tx.clone(),
                                )?;
                            }
                        }
                        KeyCode::Left => {
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            } else if app.screen.is_chat() {
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
                            if app.screen.has_prompt() || !app.screen.is_chat() {
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
                            } else if app.screen.is_chat() {
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
                            if app.screen == Screen::Runs {
                                app.watch_move(-1);
                            } else if app.screen.is_chat() {
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
                        KeyCode::Down => {
                            if app.screen == Screen::Runs {
                                app.watch_move(1);
                            } else if app.screen.is_chat() && app.history_cursor.is_some() {
                                app.input = app.history_down().unwrap_or("").to_string();
                                app.reset_cursor_to_end();
                                app.clear_completion();
                                live_area.ensure_input_cursor_visible(
                                    app.input_cursor,
                                    &app.input,
                                    app.width,
                                );
                            }
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
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            app.watch_follow_error = None;
                            abort_watch_follow(&mut watch_follow_handle);
                        }
                        app.on_run_completed(id);
                    }
                    gremlins::executor::DaemonEvent::RunFailed { id, exit_code, error } => {
                        let err_detail = error.as_deref().unwrap_or("");
                        if err_detail.is_empty() {
                            log::info!("[run failed] {id} (exit {exit_code})");
                        } else {
                            log::info!("[run failed] {id} (exit {exit_code}): {err_detail}");
                        }
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            app.watch_follow_error = None;
                            abort_watch_follow(&mut watch_follow_handle);
                        }
                        app.on_run_failed(id);
                    }
                    gremlins::executor::DaemonEvent::RunStopped { id } => {
                        log::info!("[run stopped] {id}");
                        if app.watch_following.as_deref() == Some(id.as_str()) {
                            app.watch_follow_error = None;
                            abort_watch_follow(&mut watch_follow_handle);
                        }
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
                        if let Ok(resp) = c.send_control_request(serde_json::json!({"op": "ls"})).await {
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
                app.ensure_watch_selection();
                if let Some(following) = app.watch_following.as_deref() {
                    let still_running = app
                        .active_runs
                        .get(following)
                        .map(|run| run.status == "running")
                        .unwrap_or(false);
                    if !still_running {
                        app.watch_follow_error = None;
                        abort_watch_follow(&mut watch_follow_handle);
                    }
                }
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
                    Some("view_log") => {
                        // Scope the line to the current follow generation so
                        // queued messages from an aborted or superseded follow
                        // can't leak into a newly opened pane, even for the
                        // same gremlin id.
                        let generation = raw.get("generation").and_then(|v| v.as_u64());
                        if let (Some(id), Some(line)) = (
                            raw.get("id").and_then(|v| v.as_str()),
                            raw.get("line").and_then(|v| v.as_str()),
                        ) {
                            if app.watch_following.as_deref() == Some(id)
                                && generation == Some(watch_follow_generation)
                            {
                                live_area.push_watch_line(line.to_string());
                            }
                        }
                    }
                    Some("view_error") => {
                        let generation = raw.get("generation").and_then(|v| v.as_u64());
                        if let (Some(id), Some(message)) = (
                            raw.get("id").and_then(|v| v.as_str()),
                            raw.get("message").and_then(|v| v.as_str()),
                        ) {
                            if app.watch_following.as_deref() == Some(id)
                                && generation == Some(watch_follow_generation)
                            {
                                // Ignore socket-closed notifications for
                                // terminal runs — handle_log replays the
                                // history and reaches normal EOF without a
                                // live subscription.
                                let is_terminal = app
                                    .active_runs
                                    .get(id)
                                    .is_some_and(|r| r.status != "running");
                                if !is_terminal {
                                    live_area.push_watch_line(format!("error: {message}"));
                                    app.watch_follow_error = Some(message.to_string());
                                    abort_watch_follow(&mut watch_follow_handle);
                                }
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

            // ── Session recreation completion ─────────────
            Some(result) = reset_rx.recv() => {
                app.session_recreating = false;
                match result {
                    Ok(()) => {
                        // The old chat gremlin died with the old session; the
                        // daemon reports `model: null` until the next prompt.
                        app.chat_model = None;
                        let c = Arc::clone(&client);
                        let tx = model_tx.clone();
                        tokio::spawn(async move {
                            let session_generation = c.session_generation().await;
                            let result = query_chat_model(&c, session_generation).await;
                            let _ = tx.send(result);
                        });
                    }
                    Err(e) => {
                        live_area.push_transcript_line(format!("error: {e}"));
                    }
                }
            }

            // ── Daemon chat-model lookup results ──────────
            Some((query_generation, result)) = model_rx.recv() => {
                let current_generation = client.session_generation().await;
                if query_generation != current_generation {
                    log::debug!(
                        "dropping stale chat_model query for generation {query_generation} \
                         (current {current_generation})"
                    );
                    continue;
                }
                match result {
                    Ok(model) => app.chat_model = model,
                    Err(e) => log::debug!("chat_model query failed: {e}"),
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
                    | ChatEvent::ToolOutputChunk { turn, .. }
                    | ChatEvent::ToolResult { turn, .. }
                    | ChatEvent::TurnComplete { turn, .. }
                    | ChatEvent::Done { turn, .. }
                    | ChatEvent::Ended { turn, .. }
                    | ChatEvent::Error { turn, .. } => turn.clone(),
                };
                match (&active_turn, &event_turn) {
                    (Some(active), Some(ev)) if active != ev => {
                        log::debug!("dropping stale chat event for turn {ev} (active {active})");
                        continue;
                    }
                    // No active turn means `/clear` or Esc has retired the
                    // conversation; any queued turn-tagged terminal events
                    // belong to the dead session and must not be applied.
                    (None, Some(ev)) => {
                        log::debug!("dropping retired-session chat event for turn {ev}");
                        continue;
                    }
                    _ => {}
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
                    ChatEvent::ToolOutputChunk { id, name, call, text, .. } => {
                        log::debug!(
                            "ToolOutputChunk id={id} name={name} call={call} len={}",
                            text.len()
                        );
                        live_area.push_tool_output_chunk(&id, &call, &text);
                    }
                    ChatEvent::ToolResult { id, name, call, output, .. } => {
                        log::debug!(
                            "ToolResult id={id} name={name} output_len={}",
                            output.len()
                        );
                        // Read/Edit are filtered exactly as in the TurnComplete
                        // reconciliation list: noisy file-access tools whose
                        // output belongs in the model history, not the live
                        // tool segment.
                        if name == "Read" || name == "Edit" {
                            continue;
                        }
                        live_area.commit_tool_result(&id, &call);
                    }
                    ChatEvent::TurnComplete { text, tool_calls, stats, total, .. } => {
                        log::debug!("TurnComplete text_len={} tool_calls={:?}", text.len(), tool_calls);
                        live_area.clear_tool_output();
                        // `text` is the model's full response for the just-completed
                        // turn — the same text streamed via StreamChunk. Reconcile
                        // `response_text` so the display shows the complete response
                        // even when broadcast StreamChunk events were dropped due to
                        // channel lag. `response_text` accumulates across the agent
                        // loop, so only the current turn's trailing slice is repaired.
                        live_area.reconcile_response(&text);
                        // Seed the current turn's tool-call list. ToolResult events
                        // commit each call as it finishes (streaming tools as
                        // title+tail, non-streaming tools as a `◆` event line); this
                        // list is the safety net that reconciles anything still
                        // uncommitted when the turn finishes.
                        live_area.reconcile_tool_calls(&tool_calls);
                        // The reasoning stream's title row is the live stats line:
                        // per-turn delta plus the conversation-wide total.
                        live_area.set_reasoning_title(&stats_title(
                            stats.as_ref(),
                            total.as_ref(),
                        ));
                    }
                    ChatEvent::Done { text, summary, .. } => {
                        log::debug!("Done text_len={} summary_len={} has_current_turn={}",
                            text.len(), summary.len(), live_area.has_current_turn());
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
                        live_area.clear_tool_output();

                        app.active_request = false;
                        active_turn = None;

                        // Repaint immediately so the finished response text
                        // and Done summary are visible without waiting for the
                        // next input event.
                        live_area.compose(&app)?;
                    }
                    ChatEvent::Ended { reason, .. } => {
                        if !live_area.has_current_turn() {
                            continue;
                        }
                        app.active_request = false;
                        active_turn = None;
                        let msg = format!("chat ended: {reason}");
                        live_area.finish_turn();
                        live_area.clear_tool_output();
                        live_area.push_transcript_line(msg);

                        // Repaint immediately so the ended state is visible
                        // without waiting for the next input event.
                        live_area.compose(&app)?;
                    }
                    ChatEvent::Error { message: msg, .. } => {
                        if !live_area.has_current_turn() {
                            continue;
                        }
                        app.active_request = false;
                        active_turn = None;
                        let full = format!("chat error: {msg}");
                        live_area.finish_turn();
                        live_area.clear_tool_output();
                        live_area.push_transcript_line(full);

                        // Repaint immediately so the error state is visible
                        // without waiting for the next input event.
                        live_area.compose(&app)?;
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
                    DebugEvent::ToolOutputChunk { id, name, output } => {
                        log::debug!(
                            "debug ToolOutputChunk id={id} name={name} len={}",
                            output.len()
                        );
                        // The debug socket and the chat stream are
                        // independent: do NOT route these into the chat
                        // transcript's live tool slot (which is occluded, or
                        // worse — a background chat turn would commit the
                        // debug call into its tool segment). Debug shows
                        // final results via `ToolResult` below instead.
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

    // Abort any lingering watch follow on exit.
    if let Some(handle) = watch_follow_handle.take() {
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
                // Content may contain embedded newlines; split so each vector
                // element is one logical transcript line (preserving a trailing
                // empty line produced by `split`).
                content.split('\n').map(String::from).collect()
            } else {
                vec![serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:#?}"))]
            }
        }
        "model" => {
            match (
                resp.get("model").and_then(|v| v.as_str()),
                resp.get("source").and_then(|v| v.as_str()),
            ) {
                (Some(model), Some(source)) => {
                    vec![format!("chat model: {model} (source: {source})")]
                }
                _ => {
                    vec![resp
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("chat model: not configured")
                        .to_string()]
                }
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
    use super::{build_chat_request, ctrl_c_should_quit, format_socket_response, stats_title};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn stats_title_formats_turn_delta_and_conversation_total() {
        let stats = serde_json::json!({
            "turn": 3,
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "cached_input_tokens": 40,
            "cache_creation_input_tokens": 5,
            "reasoning_tokens": 30
        });
        let total = serde_json::json!({
            "turn": 4,
            "prompt_tokens": 400,
            "completion_tokens": 200,
            "cached_input_tokens": 150,
            "cache_creation_input_tokens": 15,
            "reasoning_tokens": 120
        });

        let title = stats_title(Some(&stats), Some(&total));

        assert_eq!(
            title,
            "clanking · turn 3 · in 100 (cache 40%, +5 new) · out 50 · reason 30 · Σ out 200 · reason 120",
            "{title}"
        );
    }

    #[test]
    fn stats_title_reports_per_turn_prompt_not_conversation_total() {
        let stats = serde_json::json!({
            "turn": 2,
            "prompt_tokens": 77,
            "completion_tokens": 9,
            "cached_input_tokens": 0,
            "cache_creation_input_tokens": 3,
            "reasoning_tokens": 4
        });
        let total = serde_json::json!({
            "turn": 2,
            "prompt_tokens": 999,
            "completion_tokens": 21,
            "cached_input_tokens": 500,
            "cache_creation_input_tokens": 16,
            "reasoning_tokens": 8
        });

        let title = stats_title(Some(&stats), Some(&total));

        // The `in` figure is the per-turn prompt size (77), not the
        // conversation-wide prompt total (999), which may drop across
        // compaction and is therefore never summed.
        assert_eq!(
            title,
            "clanking · turn 2 · in 77 (cache 0%, +3 new) · out 9 · reason 4 · Σ out 21 · reason 8",
            "{title}"
        );
    }

    #[test]
    fn stats_title_renders_dashes_when_usage_is_absent() {
        let title = stats_title(None, None);

        assert_eq!(
            title,
            "clanking · turn - · in - · out - · reason - · Σ out - · reason -"
        );
    }

    #[test]
    fn stats_title_renders_zero_prompt_with_cache_dash() {
        let stats = serde_json::json!({
            "turn": 1,
            "prompt_tokens": 0,
            "completion_tokens": 7,
            "cached_input_tokens": 0,
            "cache_creation_input_tokens": 2,
            "reasoning_tokens": 1
        });

        let title = stats_title(Some(&stats), None);

        assert_eq!(
            title,
            "clanking · turn 1 · in 0 (cache -, +2 new) · out 7 · reason 1 · Σ out - · reason -"
        );
    }

    #[test]
    fn format_artifact_content_splits_embedded_newlines() {
        let resp = serde_json::json!({ "id": "gr-1", "content": "line one\nline two\n" });
        let lines = format_socket_response("artifact", &resp);
        assert_eq!(lines, vec!["line one", "line two", ""]);
    }

    #[test]
    fn format_model_response_shows_resolved_model_and_source() {
        let resp = serde_json::json!({
            "type": "ok",
            "model": "cmd/echo",
            "source": "tui.json",
        });
        let lines = format_socket_response("model", &resp);
        assert_eq!(
            lines,
            vec!["chat model: cmd/echo (source: tui.json)".to_string()]
        );
    }

    #[test]
    fn format_model_response_renders_not_configured_message() {
        let resp = serde_json::json!({
            "type": "ok",
            "model": null,
            "source": null,
            "message": "chat model: not configured",
        });
        let lines = format_socket_response("model", &resp);
        assert_eq!(lines, vec!["chat model: not configured".to_string()]);
    }

    #[test]
    fn chat_request_carries_project_root() {
        let env = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        let req = build_chat_request("push this branch", "3", "/home/user/proj", "", &env);
        assert_eq!(req["op"], "chat");
        assert_eq!(req["text"], "push this branch");
        assert_eq!(req["turn"], "3");
        assert_eq!(req["project_root"], "/home/user/proj");
        assert_eq!(req["env"]["PATH"], "/usr/bin");
        assert!(req.get("id").is_none());
    }

    #[test]
    fn chat_request_does_not_carry_a_gremlin_id() {
        let env = HashMap::new();
        let req = build_chat_request("hi", "2", "/p", "", &env);
        assert!(req.get("id").is_none());
        assert_eq!(req["project_root"], "/p");
    }

    #[test]
    fn chat_request_keeps_context_separate_from_text() {
        let env = HashMap::new();
        let req = build_chat_request("hello", "1", "/p", "Current branch: main", &env);
        assert_eq!(req["text"], "hello");
        assert_eq!(req["context"], "Current branch: main");
    }

    #[test]
    fn chat_request_omits_empty_context() {
        let env = HashMap::new();
        let req = build_chat_request("hello", "1", "/p", "", &env);
        assert!(req.get("context").is_none());
        assert_eq!(req["text"], "hello");
    }

    #[test]
    fn ctrl_c_first_press_arms_window_and_does_not_quit() {
        let mut pending = None;
        assert!(!ctrl_c_should_quit(&mut pending, Instant::now()));
        assert!(pending.is_some());
    }

    #[test]
    fn ctrl_c_second_press_within_window_quits() {
        let first = Instant::now();
        let mut pending = Some(first);
        assert!(ctrl_c_should_quit(
            &mut pending,
            first + Duration::from_millis(500)
        ));
    }

    #[test]
    fn ctrl_c_second_press_after_window_rearms_and_does_not_quit() {
        let mut pending = Some(Instant::now() - Duration::from_secs(2));
        assert!(!ctrl_c_should_quit(&mut pending, Instant::now()));
        assert!(pending.is_some());
    }
}
