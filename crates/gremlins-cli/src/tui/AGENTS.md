# TUI Module — AGENTS.md

Terminal UI for the `gremlins` CLI. Built on **ratatui** with **crossterm** for raw-mode input. Renders on the **main screen** (no alternate screen) using diff-driven double buffering; overflow content is pushed into terminal scrollback.

## File Map

| File | Role |
|------|------|
| `mod.rs` | Module root. `run()` entry point, `run_app()` event loop, debug-session plumbing (`DebugEvent`, `start_debug_session`), `format_socket_response()`. |
| `app.rs` | Pure state (`App`). Transcript/region model, active runs, conversation history, overlay state, input history. No I/O. |
| `buffer.rs` | Display buffers: `BoundedBuffer` (fixed visible-row cap, no scrollback) and `ScrollableBuffer` (FIFO that overflows into terminal scrollback). Plus `wrapped_height` helpers and `LineOrigin` provenance tags. |
| `screen.rs` | `LiveArea` — owns the main terminal screen, diff-driven double buffering, `commit_scrollback()` (renders overflow + `scroll_region_up`), raw-mode/cursor lifecycle. |
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
- **Terminal scrollback promotion.** When the `ScrollableBuffer` FIFO exceeds available rows, `overflow_split()` reports the head prefix; the caller drains it and `commit_scrollback()` renders it and calls `scroll_region_up` to push it into terminal scrollback. The bottom `RESERVED_ROWS` (2: input + info bars) are excluded from the scroll region.
- **Region model (bottom-up).** Layout: info bar → input bar → tools → reasoning → scrollable (transcript + prompt + response). Ephemeral regions (reasoning, tools) are `BoundedBuffer`s capped at 5 visible rows each and never reach scrollback. Only the `ScrollableBuffer` and the current response can overflow.
- **No `widgets.rs` / `SplitWidget`.** The old dual-section streaming widget was replaced by the region model in `buffer.rs` + `ui.rs`.
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
2. `app.submit_prompt()` flattens the previous turn into `scrollable`, starts a new turn
3. Push `"thinking..."` reasoning line, force an immediate frame
4. Set `app.active_request = true`, store `app.pending_user_message`
5. Spawn a task that sends `{"op": "chat", "text", "id"}` via the persistent `DaemonClient` (Esc sends `{"op": "cancel_chat"}`)
6. `ChatEvent::ReasoningChunk` → `app.push_reasoning_text()` (accumulates partials, flushes on newline)
7. `ChatEvent::ToolResult` → `app.push_tool()`
8. `ChatEvent::StreamChunk` → `app.push_response()` + accumulate `app.current_response`
9. `ChatEvent::Done` → reconcile `current_response` with `Done.text` (use summary only if nothing streamed), commit user+assistant to `conversation_history`, clear `active_request`
10. `Esc` during active request → abort chat task, send `cancel_chat`, discard current turn without committing

### Daemon events
`DaemonEvent` variants arrive on `event_rx`:
- `RunStarted/Completed/Failed/Stopped` → update `app.active_runs`
- `StageTransition` → update run stage
- `LogLine` → push to transcript if `app.following_log` matches, or to overlay if `overlay_log_following` matches
- `Bail` → log only

### Debug sessions
`DebugEvent` variants arrive on `debug_evt_rx` (via `recv_debug`, which pends forever when no session is active):
- `Ready` → `mark_debug_ready()`
- `TurnComplete` / `Done` / `Ended` / `StreamChunk` / `ReasoningChunk` / `ToolResult` / `Error` / `Disconnected` → push to overlay debug output, update connection state

## Rendering

`LiveArea::compose(&app)` reserves the bottom two rows for the fixed bars,
then lays the content widgets out top-to-bottom in the remaining area:
1. `Length(1)` — input bar: `> ` prompt + input text + block cursor (inverted space)
2. `Length(1)` — info bar: gremlin count, project name, log-follow indicator, key hints

The content area (everything above the two reserved rows) is subdivided
top-to-bottom: transcript → reasoning → tools. The transcript fills
(`Min(0)`), while reasoning and tools report `Length` constraints from their
visible line counts. The scrollable budget is `content_height -
reasoning_height - tools_height - fixed(prompt+response+separators)`.
Overflow is computed via `ScrollableBuffer::overflow_split()`, drained, and
returned as lines for `LiveArea::commit_scrollback()`. When the content area
has zero height, content widgets are skipped and no overflow is produced.

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
- `tokio` for async runtime, channels, spawn
- `serde_json` for daemon protocol (JSON-line socket)
- `gremlins` (core crate) for config, executor types, socket helpers