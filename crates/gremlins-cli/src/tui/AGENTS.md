# TUI Module — AGENTS.md

Terminal UI for the `gremlins` CLI. Built on **ratatui** (inline viewport, no alternate screen) with **crossterm** for raw-mode input.

## File Map

| File | Role |
|------|------|
| `mod.rs` | Module root. `run()` entry point, `run_app()` event loop, `promote_scrollback()`, `format_socket_response()`. |
| `app.rs` | Pure state (`App`). Scrollback buffer, active runs, conversation history, widget handle. No I/O. |
| `chat.rs` | Per-message chat client. Sends `{"op":"chat"}` over a fresh socket, parses streaming `ChatEvent` variants. |
| `client.rs` | Persistent `DaemonClient`. Long-lived socket for request/response + unsolicited `DaemonEvent` broadcast. Also `follow_log()` on a dedicated connection. |
| `commands.rs` | Slash-command dispatch. Returns `CommandResult` — the event loop handles each variant. |
| `editor.rs` | Stub wired to `Ctrl+G`. Appends placeholder to scrollback. |
| `ui.rs` | Ratatui render. 4-constraint vertical layout: scrollback, widget, input bar, info bar. |
| `widgets.rs` | `DynamicWidget` trait + `StreamWidget` (streaming reasoning/tool-result display, max 8 rows, freezes last 3 on turn end). |

## Architecture

```
run()
 ├─ TerminalGuard (raw mode + cursor hide, restored on drop)
 ├─ Inline viewport = terminal height
 └─ run_app()
      ├─ App::new()
      ├─ client::connect() → DaemonClient + event_rx + raw_rx
      ├─ Initial "ls" snapshot → populates app.active_runs
      ├─ Crossterm event channel (poll 20ms → mpsc)
      ├─ tokio::select! over 5 channels:
      │    1. ct_rx      — crossterm events (keys, resize)
      │    2. event_rx   — DaemonEvent broadcasts
      │    3. raw_rx     — non-event JSON from main socket
      │    4. log_rx     — log-follow lines (dedicated connection)
      │    5. result_rx  — socket-op results
      │    6. chat_rx    — ChatEvent stream
      └─ Exit: freeze widget, flush response_stream, drain scrollback → terminal.insert_before()
```

## Key Design Decisions

- **Inline viewport, no alternate screen.** Old lines are promoted to terminal scrollback via `terminal.insert_before()` when `scrollback_height > term_h * 2`. This means users can scroll up with their terminal's native scrollback.
- **Scrollback is a `Vec<(String, Style)>`.** Each line carries its own style (cyan for prompts, dark gray italic for reasoning, etc.).
- **Widget area is dynamic.** `StreamWidget` grows 0–8 rows. The layout uses `Constraint::Length(widget_h)` so the scrollback area shrinks/grows accordingly.
- **Chat uses a fresh socket per message.** The daemon processes chat as an ephemeral stage. The persistent `DaemonClient` stays free for commands during chat.
- **Log follow uses a dedicated socket.** The `log` op with `follow:true` monopolizes its connection, so it gets its own.
- **Request/response ordering is FIFO.** `DaemonClient::send_request` holds a write lock across enqueue + write so concurrent callers can't interleave.

## Event Loop Patterns

### Slash commands
`dispatch()` returns `Some(CommandResult)` → event loop match arm handles each variant:
- `Lines` → push to scrollback, promote
- `SocketOp` → spawn a task that calls `client.send_request()`, sends result to `result_tx`
- `RestartChat` → freeze widget, abort chat task, clear scrollback + history
- `Quit` → break loop
- `ShowHistory` / `TruncateHistory` → operate on `app.conversation_history`

### Chat flow
1. User types plain text → Enter
2. Echo `> text` to scrollback (cyan)
3. Create `StreamWidget`, push `"thinking..."`, set `app.widget`
4. Force immediate `terminal.draw()` so "thinking..." appears
5. Set `app.active_request = true`, store `app.pending_user_message`
6. Spawn `chat::send_message()` → `chat_tx` channel
7. `ChatEvent::ReasoningChunk` → `widget.push_str()`
8. `ChatEvent::ToolResult` → `widget.push()` with indented output
9. `ChatEvent::StreamChunk` → accumulate in `app.response_stream`, flush complete lines to scrollback
10. `ChatEvent::Done` → flush remaining stream, freeze widget tail (3 lines) to scrollback, commit user+assistant to `conversation_history`, clear `active_request`
11. `Esc` during active request → abort chat task, freeze widget, commit partial history

### Daemon events
`DaemonEvent` variants arrive on `event_rx`:
- `RunStarted/Completed/Failed/Stopped` → update `app.active_runs`, push to scrollback
- `StageTransition` → push to scrollback
- `LogLine` → push to scrollback only if `app.following_log` matches
- `Bail` → push to scrollback

## Rendering

`ui::render()` splits the frame into 4 vertical constraints:
1. `Min(0)` — scrollback (absorbs all free space). Rendered bottom-anchored: if content exceeds area, skip leading lines so the tail is visible.
2. `Length(widget_h)` — streaming widget (0 when idle, 1–8 when active)
3. `Length(1)` — input bar: `> ` prompt + input text + block cursor (inverted space)
4. `Length(1)` — info bar: gremlin count, project name, log-follow indicator, key hints

## Adding a New Slash Command

1. Add a variant to `CommandResult` in `commands.rs` (or reuse `Lines`/`SocketOp`)
2. Add a match arm in `dispatch()`
3. Add a match arm in the `CommandResult` handler in `mod.rs` `run_app()`
4. Update `help_text()` in `commands.rs`

## Adding a New ChatEvent

1. Add variant to `ChatEvent` in `chat.rs`
2. Add parsing in `parse_chat_event()`
3. Add match arm in the `chat_rx` handler in `mod.rs`

## Dependencies

- `ratatui` 0.30 with `crossterm` + `scrolling-regions` features
- `crossterm` 0.29 for raw mode, events, cursor control
- `tokio` for async runtime, channels, spawn
- `serde_json` for daemon protocol (JSON-line socket)
- `gremlins` (core crate) for config, executor types, socket helpers