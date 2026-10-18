# TUI Module — AGENTS.md

Terminal UI for the `gremlins` CLI. Built on **ratatui** with **crossterm** for raw-mode input. Renders on the **main screen** (no alternate screen) using diff-driven double buffering; overflow content is pushed into terminal scrollback.

## File Map

| File | Role |
|------|------|
| `mod.rs` | Module root. `run()` entry point, `run_app()` event loop, debug-session plumbing (`DebugEvent`, `start_debug_session`), `format_socket_response()`. |
| `app.rs` | Pure state (`App`). Active runs, chat id, full-screen marker (`Screen`), input history, cursor state. No I/O, no widget ownership. |
| `buffer.rs` | Display buffer: `LineBuffer` (styled FIFO, with drain-head and wrapping helpers). Plus `wrapped_height` and `line_wrapped_height` utilities. |
| `markdown.rs` | Markdown-to-ratatui renderer. Turns a markdown string into `Vec<Line<'static>>` of styled spans via `pulldown-cmark` (tables, strikethrough, task lists). |
| `screen.rs` | `LiveArea` — owns the main terminal screen, the typed widget `Scene` (`transcript`/`reasoning`/`input_bar`/`info_bar` fields plus a typed `ScreenView` enum), diff-driven double buffering, `commit_scrollback()` (renders overflow + `scroll_region_up`), raw-mode/cursor lifecycle. Also defines the full-screen `RunsView` body with its inline watch stream pane. |
| `widgets.rs` | `LiveAreaWidget` trait + the shared recursive layout engine + concrete widgets: composite transcript (single styled FIFO + current-turn draft state + one embedded tool `StreamingWidget` child), the `StreamingWidget` primitive, input bar, info bar, and the full-screen `DebugWidget`. |
| `chat.rs` | Per-message chat client. Sends `{"op":"chat"}` over a fresh socket, parses streaming `ChatEvent` variants. |
| `client.rs` | Persistent `DaemonClient`. Long-lived socket for request/response + unsolicited `DaemonEvent` broadcast. Also `follow_log()` on a dedicated connection. |
| `commands.rs` | Slash-command dispatch. Returns `CommandResult` — the event loop handles each variant. |
| `editor.rs` | Stub wired to `Ctrl+G`. Appends placeholder to transcript. |

## Architecture

```
run()
 ├─ Refuse non-TTY stdout
 ├─ config::init_global() (fatal config errors report on real stderr)
 ├─ LiveArea::new() (raw mode + cursor hide + clear main screen)
 ├─ Dup real stderr with FD_CLOEXEC
 ├─ Redirect stderr → state_root/tui.log
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

On a fatal `run_app()` error after the stderr redirect, `run()` restores the
terminal and the saved stderr, writes the `gremlins: `-prefixed error to
stdout (so it remains visible even when stderr was redirected at launch), and
returns `RunError::Reported` so the CLI exits nonzero without printing it again.
```

## Key Design Decisions

- **Main screen, not alternate screen.** `LiveArea` owns the primary terminal screen. On exit nothing is restored from an alternate buffer — content stays in terminal scrollback, matching normal CLI behavior.
- **Diff-driven double buffering.** `LiveArea` keeps `curr`/`prev` screen-sized `Buffer`s and flushes only changed cells each frame (`prev.diff_iter(&curr)`). `invalidate()` forces a full repaint.
- **One recursive layout engine.** Both the top-level compose and the debug widget call the same engine over an ordered slice of `&mut dyn LiveAreaWidget` (`finalize → measure → allocate → drain overflow → paint`). Width travels down as an argument and height travels up; a container sums its children's heights, then re-runs allocation on its children constrained to its allocated row budget. The transcript no longer uses the recursive engine for its tool children — it flat-sums their desired heights and allocates them directly.
- **Typed scene, per-frame type erasure.** `LiveArea` stores `Scene` (concrete `TranscriptWidget`, `InputBarWidget`, `InfoBarWidget`, and an `Option<ScreenView>`). Each frame takes the scene out, erases the selected fields to `&mut dyn LiveAreaWidget` for the layout engine, and puts it back. There is no concrete type reification or long-lived boxed-widget owner.
- **Terminal scrollback promotion.** When the transcript FIFO exceeds its allocated rows, `TranscriptWidget::take_overflow()` drains the head and returns the overflow lines; `commit_scrollback()` renders them and calls `scroll_region_up` to push them into terminal scrollback. The pin-to-screen rows are the input and info bars only; while streaming, the top-level reasoning band and the transcript's embedded tool `StreamingWidget` slots are display-only and never reach scrollback. The FIFO is the sole overflow producer.
- **Single styled FIFO + top-level reasoning band.** The transcript is one styled FIFO and is the only render source for the transcript region. Prompt, response, tool-call, and system lines are pushed into it as styled lines as they arrive. Prompt and response text are markdown-rendered; committed tool blocks (`◆` title + tail) are inserted at the response boundary via the uniform [`commit_tool_stream_block`] path. Live tool output renders as `StreamingWidget` children **below** the FIFO — not embedded between bands within it. When a tool arrives and the response accumulator has accumulated text, that text is **sealed** into the FIFO before the tool widget is created, so model text that arrived before the tool call always appears before the tool's `◆` line. Live reasoning stays out of the FIFO as a top-level bottom band pinned directly above the input bar (a fixed-height, tail-anchored `StreamingWidget` that collapses to zero when idle). At `finish_turn`, the transcript first flushes any uncommitted tool-call entries and rebuilds the response, then the scene commits only the reasoning title line as the final FIFO line of the turn; the reasoning body is display-only and cleared with the live band. Finished history reads prompt → sealed text blocks → committed tool blocks → response → reasoning title. In-progress streams stay display-only and never enter the FIFO or terminal scrollback; only the committed tool tails and the final reasoning title become permanent transcript history.
- **Screen switch, not overlays.** `App` owns a `Screen` enum (`Chat`, `Runs`, `Debug { id }`). `LiveArea` mirrors that in a typed `ScreenView` enum. `LiveArea::compose()` composes the chat scene for `Screen::Chat`, or the single active full-screen view body for any other screen. Full-screen views never commit their content to chat scrollback, and `Esc` (or the first `Ctrl+C` press) exits back to chat.
- **Full-screen views.** `RunsView` renders the running-gremlin watch table and owns an optional inline live-log pane, and `DebugWidget` renders the interactive debug screen while owning its prompt/history/connection state. Each full-screen view is flexible and fills the whole terminal.
- **Uniform tool path.** Every tool — streaming or not — creates a `StreamingWidget` and commits via [`commit_tool_stream_block`]. Tools with an empty body commit a title-only block (functionally identical to the old diamond-line behavior); streaming tools commit their title plus the retained 5-row tail. `Read`/`Edit`/`Write`/`Glob`/`Grep` are no longer filtered — all tools participate uniformly. Live tool slots are `STREAM_MAX_ROWS`-capped, tail-anchored, lossy head-trimmed, ANSI-stripped, collapse to zero when idle, and never enter the transcript FIFO or terminal scrollback while streaming. The three-slice FIFO split (pre/events/tail) is gone; the FIFO renders as one contiguous tail-anchored block, with its rendered height tight to its content so live tool widgets sit directly beneath with no gap. **Sub-agent (`Task`) tool output is not forwarded:** nested loops run with `stream_events = None`, so their events never reach the parent TUI; `make_task_runner` therefore clears the inherited live-output sender (`child_ctx.tool_output = None`) so a sub-agent's chunks can't create a tool slot with no matching close event.
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
- `Debug(id)` → enter `Screen::Debug { id }` + `start_debug_session()`

### Chat flow
1. User types plain text → Enter
2. `live_area.submit_prompt()` pushes a separator and the markdown-rendered prompt into the FIFO, then pins the turn start; the top-level reasoning band and the embedded tool slots are cleared with the new prompt
3. Force an immediate frame so the pinned prompt appears before the first daemon event
4. Set `app.active_request = true`, store `app.pending_user_message`
5. Spawn a task that sends `{"op": "chat", "text", "id"}` via the persistent `DaemonClient` (Esc sends `{"op": "cancel_chat"}`)
6. `ChatEvent::ReasoningChunk` → `live_area.push_reasoning_text()` (appends to the top-level reasoning band; partials are flushed on newline)
7. `ChatEvent::ToolOutputChunk` → `live_area.push_tool_output_chunk(id, call, text)` creates or updates the live stream keyed by call id (sealing any accumulated response text first); `ChatEvent::ToolResult` → `live_area.commit_tool_result(id, call)` commits that stream's title plus retained 5-row tail into the transcript via the uniform `commit_tool_stream_block` path. `ChatEvent::TurnComplete` seeds the turn's tool-call list for end-of-turn reconciliation; all tools participate uniformly
8. `ChatEvent::StreamChunk` → `live_area.push_response()` (accumulates raw response, marks the response region dirty)
9. `ChatEvent::Done` → reconcile the raw response accumulator with `Done.text` (use summary only if nothing streamed), then `live_area.finish_turn()` flushes any uncommitted tool-call list entries, rebuilds the response, unpins the turn, clears its draft state plus the live reasoning/tool slots, and commits the reasoning title as the final FIFO line of the finished turn (the reasoning body is discarded; committed tool blocks and the title stay in the transcript FIFO forever)
10. `Esc` during active request → abort chat task, send `cancel_chat`, `live_area.clear_current_turn()` truncates the FIFO back to the turn start

### Daemon events
`DaemonEvent` variants arrive on `event_rx`:
- `RunStarted/Completed/Failed/Stopped` → update `app.active_runs`; terminal states also release a watch follow socket for the followed id
- `StageTransition` → update run stage
- `LogLine` → push to transcript if `app.following_log` matches (independent `/log <id>` following)
- `Bail` → log only

### Debug sessions
`DebugEvent` variants arrive on `debug_evt_rx` (via `recv_debug`, which pends forever when no session is active):
- `Ready` → `live_area.mark_debug_ready()`
- `TurnComplete` / `Done` / `Ended` / `StreamChunk` / `ReasoningChunk` / `ToolResult` / `Error` / `Disconnected` → `live_area.push_debug_output()`, update connection state via `live_area.set_debug_connected()`
- `ToolOutputChunk` is **ignored** — debug tool output is never routed into the chat transcript's live tool slot (the two sockets are independent, and a background chat turn must not commit a debug call into its tool segment). Debug shows final results via `ToolResult`.

## Rendering

`LiveArea::compose(&app)` takes the typed `Scene` out and runs the recursive
layout engine over an ordered `Vec<&mut dyn LiveAreaWidget>`. For chat the
order is transcript, reasoning, input bar, info bar; for any non-chat screen
it is only the active `ScreenView` body. The engine performs `finalize →
measure → allocate → drain overflow → paint`. Allocated widths are passed
down as arguments (`finalize(width)`, `min_height(width, …)`, etc.); a
container owns margin policy and hands children the already-reduced inner
width. Top-level siblings instead apply their own one-cell outer gutter so
they align: the transcript insets its `render` by one cell, and the reasoning
band mirrors that inset via `StreamingWidget::with_left_margin(1)`.
The input and info bars are the last two chat widgets and are excluded from
the scroll region; the reserved rows are derived from their allocated heights.
The reasoning band sits immediately above them and is inside the scrollable
content region.

The transcript fills the remaining content area and is itself a container.
After the outer allocate pass gives it `H` rows, it re-runs the same engine
on its ephemeral tool children — one `StreamingWidget` per in-flight
tool-call id — constrained to the ephemeral band, then paints bands
top-to-bottom — pre-tool FIFO head (prior history and the current prompt,
top-anchored), the FIFO tool-call-event band (committed tool blocks,
top-anchored), the embedded live-tool streams (streaming children capped at 5
rows, tail-anchored and display-lossy while retaining a 5-row commit tail),
and the streaming-response FIFO tail — all within the transcript's one-cell
inner margin. Only the transcript FIFO drains overflow lines for
`LiveArea::commit_scrollback()`; each `StreamingWidget` self-trims and
returns no overflow.

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