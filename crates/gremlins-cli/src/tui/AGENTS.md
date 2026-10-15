# TUI Module — AGENTS.md

Terminal UI for the `gremlins` CLI. Built on **ratatui** with **crossterm** for raw-mode input. Renders on the **main screen** (no alternate screen) using diff-driven double buffering; overflow content is pushed into terminal scrollback.

## File Map

| File | Role |
|------|------|
| `mod.rs` | Module root. `run()` entry point, `run_app()` event loop, debug-session plumbing (`DebugEvent`, `start_debug_session`), `format_socket_response()`. |
| `app.rs` | Pure state (`App`). Active runs, chat id, full-screen marker (`Screen`), input history, cursor state. No I/O, no widget ownership. |
| `buffer.rs` | Display buffer: `LineBuffer` (styled FIFO, with drain-head and wrapping helpers). Plus `wrapped_height` and `line_wrapped_height` utilities. |
| `markdown.rs` | Markdown-to-ratatui renderer. Turns a markdown string into `Vec<Line<'static>>` of styled spans via `pulldown-cmark` (tables, strikethrough, task lists). |
| `screen.rs` | `LiveArea` — owns the main terminal screen, the typed widget `Scene` (`transcript`/`input_bar`/`info_bar` fields plus a typed `ScreenView` enum), diff-driven double buffering, `commit_scrollback()` (renders overflow + `scroll_region_up`), raw-mode/cursor lifecycle. Also defines the full-screen `RunsView` / `LogView` bodies. |
| `widgets.rs` | `LiveAreaWidget` trait + the shared recursive layout engine + concrete widgets: composite transcript (single styled FIFO + current-turn draft state + two embedded `StreamingWidget` children), the `StreamingWidget` primitive, input bar, info bar, and the full-screen `DebugWidget`. |
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
- **One recursive layout engine.** Both the top-level compose and the transcript's ephemeral-band layout call the same engine over an ordered slice of `&mut dyn LiveAreaWidget` (`finalize → measure → allocate → drain overflow → paint`). Width travels down as an argument and height travels up; a container sums its children's heights, then re-runs allocation on its children constrained to its allocated row budget.
- **Typed scene, per-frame type erasure.** `LiveArea` stores `Scene` (concrete `TranscriptWidget`, `InputBarWidget`, `InfoBarWidget`, and an `Option<ScreenView>`). Each frame takes the scene out, erases the selected fields to `&mut dyn LiveAreaWidget` for the layout engine, and puts it back. There is no concrete type reification or long-lived boxed-widget owner.
- **Terminal scrollback promotion.** When the transcript FIFO exceeds its allocated rows, `TranscriptWidget::take_overflow()` drains the head and returns the overflow lines; `commit_scrollback()` renders them and calls `scroll_region_up` to push them into terminal scrollback. The pin-to-screen rows (input + info bars) are excluded from the scroll region; the embedded `StreamingWidget` slots are display-only and never reach scrollback. The FIFO is the sole overflow producer.
- **Single styled FIFO + nested streaming children.** The transcript is one styled FIFO and is the only render source for the transcript region. Prompt, response, tool-call, and system lines are pushed into it as styled lines as they arrive. Prompt and response text are markdown-rendered; tool-call lines are rendered with the deco manifest marker (chrome `◆` prefix + nickel italic tool name) and are inserted above the streaming response region. Reasoning and live tool output are **not** part of the transcript FIFO: they live in embedded `StreamingWidget` children (brass diamond + nickel italic styling for reasoning; the tool slot renders a `◆ `-prefixed title row followed by four-space-indented, nickel italic, ANSI-stripped tool output). During a turn the transcript re-runs the shared layout engine over those children and paints the FIFO pre-reasoning head, reasoning, the FIFO tool-call-event band, live tool output, and the streaming-response tail top-to-bottom (prompt → reasoning → tool-call events → live tool output → response). Streaming content never enters the FIFO or terminal scrollback; it is cleared on finish/cancel/clear, so finished history reads prompt → tool → answer with no thinking in between.
- **Screen switch, not overlays.** `App` owns a `Screen` enum (`Chat`, `Runs`, `Log { id }`, `Debug { id }`). `LiveArea` mirrors that in a typed `ScreenView` enum. `LiveArea::compose()` composes the chat scene for `Screen::Chat`, or the single active full-screen view body for any other screen. Full-screen views never commit their content to chat scrollback, and `Esc` (or the first `Ctrl+C` press) exits back to chat.
- **Full-screen views.** The runs table and single-gremlin log viewer are rendered by `RunsView` / `LogView` (owned by the screen module), and `DebugWidget` renders the interactive debug screen while owning its prompt/history/connection state. Each full-screen view is flexible and fills the whole terminal.
- **Embedded live tool slot.** `Bash`/`WebSearch` live output lives inside `TranscriptWidget` as a configured `StreamingWidget` sibling of the reasoning slot, ordered prompt → reasoning → tool-call events → live tool output → response. Both streaming slots are capped at `REASONING_MAX_ROWS`, tail-anchored, lossy head-trimmed (ANSI-stripped for tool output), collapse to zero when idle, and never enter the transcript FIFO or terminal scrollback.
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
- `ShowRuns` → enter `Screen::Runs`
- `ShowLog(id)` → enter `Screen::Log { id }` and spawn a dedicated log follow
- `Debug(id)` → enter `Screen::Debug { id }` + `start_debug_session()`

### Chat flow
1. User types plain text → Enter
2. `live_area.submit_prompt()` pushes a separator and the markdown-rendered prompt into the FIFO, then pins the turn start and clears the embedded reasoning slot
3. Force an immediate frame so the pinned prompt appears before the first daemon event
4. Set `app.active_request = true`, store `app.pending_user_message`
5. Spawn a task that sends `{"op": "chat", "text", "id"}` via the persistent `DaemonClient` (Esc sends `{"op": "cancel_chat"}`)
6. `ChatEvent::ReasoningChunk` → `live_area.push_reasoning_text()` (delegates to the transcript's embedded reasoning slot; partials are flushed on newline)
7. `ChatEvent::ToolResult` → matched and logged only (no rendering); `ChatEvent::TurnComplete` is the sole source of rich tool-event lines, which are inserted into the transcript above the streaming response (Read/Edit calls filtered)
8. `ChatEvent::StreamChunk` → `live_area.push_response()` (accumulates raw response, marks the response region dirty)
9. `ChatEvent::Done` → reconcile the raw response accumulator with `Done.text` (use summary only if nothing streamed), then `live_area.finish_turn()` unpins the turn and clears its draft state and embedded reasoning/tool slots (tool-call lines stay in the transcript FIFO forever)
10. `Esc` during active request → abort chat task, send `cancel_chat`, `live_area.clear_current_turn()` truncates the FIFO back to the turn start

### Daemon events
`DaemonEvent` variants arrive on `event_rx`:
- `RunStarted/Completed/Failed/Stopped` → update `app.active_runs`
- `StageTransition` → update run stage
- `LogLine` → push to transcript if `app.following_log` matches, or to the full-screen log view if `log_view_following` matches
- `Bail` → log only

### Debug sessions
`DebugEvent` variants arrive on `debug_evt_rx` (via `recv_debug`, which pends forever when no session is active):
- `Ready` → `live_area.mark_debug_ready()`
- `TurnComplete` / `Done` / `Ended` / `StreamChunk` / `ReasoningChunk` / `ToolResult` / `Error` / `Disconnected` → `live_area.push_debug_output()`, update connection state via `live_area.set_debug_connected()`

## Rendering

`LiveArea::compose(&app)` takes the typed `Scene` out and runs the recursive
layout engine over an ordered `Vec<&mut dyn LiveAreaWidget>`. For chat the
order is transcript, input bar, info bar; for any non-chat screen it is only
the active `ScreenView` body. The engine performs `finalize → measure →
allocate → drain overflow → paint`. Allocated widths are passed down as
arguments (`finalize(width)`, `min_height(width, …)`, etc.); the parent owns
margin policy and hands children the already-reduced inner width. The input
and info bars are the last two chat widgets and are excluded from the
scroll region; the reserved rows are derived from their allocated heights.

The transcript fills the remaining content area and is itself a container.
After the outer allocate pass gives it `H` rows, it re-runs the same engine
on its two `StreamingWidget` children constrained to the ephemeral band, then
paints bands top-to-bottom — pre-reasoning FIFO head (prior history and the
current prompt, top-anchored), the embedded reasoning stream, the FIFO
tool-call-event band (top-anchored), the embedded live-tool stream (both
streaming children capped at 5 rows, tail-anchored and lossy), and the
streaming-response FIFO tail — all within the transcript's one-cell inner
margin. Only the transcript FIFO drains overflow lines for
`LiveArea::commit_scrollback()`; each `StreamingWidget` self-trims and returns
no overflow.

When a non-chat screen is active, `LiveArea` composes only that screen's
typed `ScreenView` body across the whole terminal; the chat scene
(transcript, input, info bars) is occluded and no overflow is committed.

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