# TUI Module — AGENTS.md

Terminal UI for the `gremlins` CLI. Built on **ratatui** with **crossterm** for raw-mode input. Renders on the **main screen** (no alternate screen) using diff-driven double buffering; overflow content is pushed into terminal scrollback.

## File Map

| File | Role |
|------|------|
| `mod.rs` | Module root. `run()` entry point, `run_app()` event loop, debug-session plumbing (`DebugEvent`, `start_debug_session`), `format_socket_response()`. |
| `app.rs` | Pure state (`App`). Active runs, chat id, overlay mode marker, input history, cursor state. No I/O, no widget ownership. |
| `buffer.rs` | Display buffer: `LineBuffer` (styled FIFO, with drain-head and wrapping helpers). Plus `wrapped_height` and `line_wrapped_height` utilities. |
| `markdown.rs` | Markdown-to-ratatui renderer. Turns a markdown string into `Vec<Line<'static>>` of styled spans via `pulldown-cmark` (tables, strikethrough, task lists). |
| `screen.rs` | `LiveArea` — owns the main terminal screen, the dynamic widget stack (`Vec<Box<dyn LiveAreaWidget>>`), diff-driven double buffering, `commit_scrollback()` (renders overflow + `scroll_region_up`), raw-mode/cursor lifecycle. |
| `widgets.rs` | `LiveAreaWidget` trait + concrete widgets: transcript (the single styled FIFO + current-turn draft state), input bar, info bar, and the three overlay widgets (watch table, watch single, debug). |
| `chat.rs` | Per-message chat client. Sends `{"op":"chat"}` over a fresh socket, parses streaming `ChatEvent` variants. |
| `client.rs` | Persistent `DaemonClient`. Long-lived socket for request/response + unsolicited `DaemonEvent` broadcast. Also `follow_log()` on a dedicated connection. |
| `commands.rs` | Slash-command dispatch. Returns `CommandResult` — the event loop handles each variant. |
| `editor.rs` | Stub wired to `Ctrl+G`. Appends placeholder to transcript. |

## Architecture

```
run()
 ├─ Refuse non-TTY stdout
 ├─ Redirect stderr → state_root/tui.log
 ├─ LiveArea::new() (raw mode + cursor hide + clear main screen)
 └─ run_app()
      ├─ App::new()
      ├─ client::connect() → DaemonClient + event_rx + raw_rx
      ├─ Initial "ls" snapshot → populates app.active_runs
      ├─ Crossterm event channel (poll 20ms → mpsc)
      ├─ Per-frame: live_area.compose(&app) → render widgets → commit overflow → draw
      ├─ tokio::select! over 8 channels:
      │    1. ct_rx        — crossterm events (keys, resize)
      │    2. event_rx     — DaemonEvent broadcasts
      │    3. raw_rx       — non-event JSON from main socket
      │    4. snapshot_rx  — run-snapshot refresh (after event_lagged)
      │    5. log_rx       — log-follow lines (dedicated connections)
      │    6. result_rx    — socket-op results
      │    7. chat_rx      — ChatEvent stream
      │    8. debug_evt_rx — DebugEvent stream (recv_debug, pending when idle)
      └─ Exit: commit in-flight turn to history, abort log/debug tasks, client.shutdown()
```

## Key Design Decisions

- **Main screen, not alternate screen.** `LiveArea` owns the primary terminal screen. On exit nothing is restored from an alternate buffer — content stays in terminal scrollback, matching normal CLI behavior.
- **Diff-driven double buffering.** `LiveArea` keeps `curr`/`prev` screen-sized `Buffer`s and flushes only changed cells each frame (`prev.diff_iter(&curr)`). `invalidate()` forces a full repaint.
- **Terminal scrollback promotion.** When the `ScrollableBuffer` FIFO exceeds available rows, `overflow_split()` reports the head prefix; the caller drains it and `commit_scrollback()` renders it and calls `scroll_region_up` to push it into terminal scrollback. The bottom `RESERVED_ROWS` (2: input + info bars) are excluded from the scroll region. A pinned boundary (the current turn's start) ensures an in-progress turn never reaches terminal scrollback — only completed turns and the separator before a new turn may overflow.
- **Single styled FIFO.** The transcript is one `ScrollableBuffer` of styled lines and is the only render source for the transcript region. Prompt, response, and system lines are pushed into it as styled lines as they arrive. Prompt and response text are markdown-rendered. Reasoning and tool lines are **not** part of the transcript FIFO: they live in their own `ReasoningWidget` and `ToolEventsWidget` buffers (dark-gray italic styling) and are cleared when the next prompt is submitted. The current turn's draft state (raw response accumulator, turn-start/response-start markers, response-dirty flag) lives on the transcript widget; the FIFO is the only thing `render` reads.
- **Dynamic widget stack.** `LiveArea` owns an ordered `Vec<Box<dyn LiveAreaWidget>>`. The initial stack (top-to-bottom) is transcript → input bar → info bar. Overlays are pushed/popped at the top of the stack via `push_overlay`/`pop_overlay`. `compose()` lays every widget out top-to-bottom using each widget's `min_height(width, ctx)` floor and `flexible()` flag.
- **Overlay widgets.** Three overlay widgets implement `LiveAreaWidget`: `WatchTableWidget` (non-blocking, reads the run table from the render context), `WatchSingleWidget` (blocking log viewer owning its log buffer), and `DebugWidget` (blocking debug session owning prompt/history/connection state). Blocking overlays collapse the transcript to zero height so they occupy the whole content region while the fixed input/info bars stay visible. Overlay-local state is cleared when the widget is popped.
- **Chat uses a fresh socket per message.** The daemon processes chat as an ephemeral stage. The persistent `DaemonClient` stays free for commands during chat.
- **Log follow uses a dedicated socket.** The `log` op with `follow:true` monopolizes its connection, so it gets its own (`client::follow_log`).
- **Debug sessions use a dedicated socket.** `/debug <id>` opens a fresh socket with its own reader task and command channel (`talk`/`quit`/`bail`/`continue`).
- **Request/response ordering is FIFO.** `DaemonClient::send_request` holds a write lock across enqueue + write so concurrent callers can't interleave.

## Event Loop Patterns

### Slash commands
`dispatch()` returns `Some(CommandResult)` → event loop match arm handles each variant:
- `Lines` → push to transcript
- `SocketOp` → spawn a task that calls `client.send_request()`, sends result to `result_tx` (or starts a log follow for `op == "log"`)
- `RestartChat` → clear current turn, abort chat task, clear history + input
- `Quit` → break loop
- `ShowHistory` / `TruncateHistory` → operate on `app.conversation_history`
- `ToggleWatch` / `WatchSingle(id)` → open/close watch overlays (single-watch spawns a dedicated log follow)
- `Debug(id)` → open debug overlay + `start_debug_session()`

### Chat flow
1. User types plain text → Enter
2. `live_area.submit_prompt()` pushes a separator and the markdown-rendered prompt into the FIFO, then pins the turn start
3. Push `"thinking..."` reasoning line, force an immediate frame
4. Set `app.active_request = true`, store `app.pending_user_message`
5. Spawn a task that sends `{"op": "chat", "text", "id"}` via the persistent `DaemonClient` (Esc sends `{"op": "cancel_chat"}`)
6. `ChatEvent::ReasoningChunk` → `live_area.push_reasoning_text()` (accumulates partials, flushes on newline)
7. `ChatEvent::ToolResult` → matched and logged only (no rendering); `ChatEvent::TurnComplete` is the sole source of rich tool-event lines
8. `ChatEvent::StreamChunk` → `live_area.push_response()` (accumulates raw response, marks the response region dirty)
9. `ChatEvent::Done` → reconcile the raw response accumulator with `Done.text` (use summary only if nothing streamed), then `live_area.finish_turn()` unpins the turn and clears its draft state (reasoning and tool content stays in its own widgets; the next prompt clears them)
10. `Esc` during active request → abort chat task, send `cancel_chat`, `live_area.clear_current_turn()` truncates the FIFO back to the turn start

### Daemon events
`DaemonEvent` variants arrive on `event_rx`:
- `RunStarted/Completed/Failed/Stopped` → update `app.active_runs`
- `StageTransition` → update run stage
- `LogLine` → push to transcript if `app.following_log` matches, or to overlay if `overlay_log_following` matches
- `Bail` → log only

### Debug sessions
`DebugEvent` variants arrive on `debug_evt_rx` (via `recv_debug`, which pends forever when no session is active):
- `Ready` → `live_area.mark_debug_ready()`
- `TurnComplete` / `Done` / `Ended` / `StreamChunk` / `ReasoningChunk` / `ToolResult` / `Error` / `Disconnected` → `live_area.push_debug_output()`, update connection state via `live_area.set_debug_connected()`

## Rendering

`LiveArea::compose(&app)` lays every widget in the stack out top-to-bottom
in a single ratatui layout pass. The bottom two rows are the input and info
bars (the last two widgets, each `Length(1)`), excluded from the scroll
region:
1. `Length(1)` — input bar: `> ` prompt + input text + block cursor (inverted space)
2. `Length(1)` — info bar: gremlin count, project name, log-follow indicator, key hints

The transcript fills the content area (`Min(0)`). Its `render` rebuilds the
response region if dirty, splits overflow with the current turn pinned,
drains the head, adjusts the markers by the drained count, and renders the
FIFO tail. Overflow is computed via `ScrollableBuffer::overflow_split()`,
drained, and returned as lines for `LiveArea::commit_scrollback()`. Only the
transcript may return overflow; overlay widgets always return an empty `Vec`.

When a blocking overlay is active, the transcript collapses to zero height
(its `min_height` returns `0` when `ctx.blocking_overlay()`),
so the overlay fills the whole content region while the input/info bars
remain visible. The non-blocking watch table reserves an upper portion and
leaves the transcript below it visible.

## Adding a New Slash Command

1. Add a variant to `CommandResult` in `commands.rs` (or reuse `Lines`/`SocketOp`)
2. Add a match arm in `dispatch()`
3. Add a match arm in the `CommandResult` handler in `mod.rs` `run_app()`
4. Update `help_text()` in `commands.rs`

## Adding a New ChatEvent

1. Add variant to `ChatEvent` in `chat.rs`
2. Add parsing in `parse_chat_event()`
3. Add match arm in the `chat_rx` handler in `mod.rs`

## Adding a New DebugEvent

1. Add variant to `DebugEvent` in `mod.rs`
2. Add parsing in `parse_debug_event()`
3. Add match arm in the `debug_evt_rx` handler in `mod.rs`

## Dependencies

- `ratatui` 0.30 with `crossterm` feature
- `crossterm` 0.29 for raw mode, events, cursor control, scroll region
- `pulldown-cmark` 0.13 for markdown rendering (tables, strikethrough, task lists)
- `tokio` for async runtime, channels, spawn
- `serde_json` for daemon protocol (JSON-line socket)
- `gremlins` (core crate) for config, executor types, socket helpers