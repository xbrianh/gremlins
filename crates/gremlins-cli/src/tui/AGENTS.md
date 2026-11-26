# TUI Module — AGENTS.md

Terminal UI for the `gremlins` CLI. Built on **ratatui** with **crossterm** for raw-mode input. Renders on the **main screen** (no alternate screen) using diff-driven double buffering; overflow content is pushed into terminal scrollback.

## File Map

| File | Role |
|------|------|
| `mod.rs` | Module root. `run()` entry point, `run_app()` event loop, bus-selected conversation routing, exact invocation resolution, lazy native input, and receipt handling, `format_socket_response()`. |
| `app.rs` | Pure state (`App`). Active runs, chat id, full-screen marker (`Screen`), input history, cursor state. No I/O, no widget ownership. |
| `buffer.rs` | Display buffer: `LineBuffer` (styled FIFO, with drain-head and wrapping helpers). Plus `wrapped_height` and `line_wrapped_height` utilities. |
| `markdown.rs` | Markdown-to-ratatui renderer. Turns a markdown string into `Vec<Line<'static>>` of styled spans via `pulldown-cmark` (tables, strikethrough, task lists). |
| `screen.rs` | `LiveArea` — owns the main terminal screen, the typed widget `Scene` (`transcript`/`reasoning`/`input_bar`/`info_bar` fields plus a typed `ScreenView` enum), diff-driven double buffering, `commit_scrollback()` (renders overflow + `scroll_region_up`), raw-mode/cursor lifecycle. Also defines the full-screen `RunsView` body with its inline watch stream pane. |
| `widgets.rs` | `LiveAreaWidget` trait + the shared recursive layout engine + concrete widgets: composite transcript (single styled FIFO + current-turn draft state + one embedded tool `StreamingWidget` child), the `StreamingWidget` primitive, input bar, info bar. Selected conversations share these widgets. |
| `client.rs` | Persistent `DaemonClient`. Long-lived socket for request/response + unsolicited `DaemonEvent` broadcast. Also dedicated `follow_log()` and passive native `follow_run()` connections. |
| `../input.rs` | Native exact-execution input transport. Binds lazily for commands/corrections and reports command receipts independently of bus output. |
| `watch_replay.rs` | Watch history/live handoff: exact complete native replay; incomplete replay displays a persisted tail and all original native frames without deduplication, then follows the original subscription. |
| `protocol.rs` | Shared bus selectors and exact stage_run_id identities. Selection is independent of native input binding. |
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
      ├─ client::connect() → DaemonClient + event_rx + chat_rx + raw_rx
      ├─ Initial "ls" snapshot → populates app.active_runs
      ├─ Crossterm event channel (poll 20ms → mpsc)
      ├─ Per-frame: live_area.compose(&app) → render widgets → commit overflow → draw
      ├─ tokio::select! over input, daemon, and operation channels:
      │    1. ct_rx        — crossterm events (keys, resize)
      │    2. event_rx     — DaemonEvent broadcasts
      │    3. raw_rx       — non-event JSON from main socket
      │    4. status_rx    — run-status refresh
      │    5. log_rx       — log-follow lines (dedicated connections)
      │    6. result_rx    — socket-op results
      │    7. chat_rx      — home chat events with turn/stage_run_id identity
      │    8. selected bus stream — replay/live invocation output
      │    9. delivery_rx  — correction delivery receipts
      │   10. reset_rx / model_rx — session reset and model-query results
      └─ Exit: commit in-flight turn to history, release local input/log tasks, client.shutdown()

On a fatal `run_app()` error after the stderr redirect, `run()` restores the
terminal and the saved stderr, writes the `gremlins: `-prefixed error to
stdout (so it remains visible even when stderr was redirected at launch), and
returns `RunError::Reported` so the CLI exits nonzero without printing it again.
```

## Key Design Decisions

- **Main screen, not alternate screen.** `LiveArea` owns the primary terminal screen. On exit nothing is restored from an alternate buffer — content stays in terminal scrollback, matching normal CLI behavior.
- **Diff-driven double buffering.** `LiveArea` keeps `curr`/`prev` screen-sized `Buffer`s and flushes only changed cells each frame (`prev.diff_iter(&curr)`). `invalidate()` forces a full repaint.
- **One recursive layout engine.** Top-level compose uses the shared engine over an ordered slice of `&mut dyn LiveAreaWidget` (`finalize → measure → allocate → drain overflow → paint`). Width travels down as an argument and height travels up; a container sums its children's heights, then re-runs allocation on its children constrained to its allocated row budget. The transcript no longer uses the recursive engine for its tool children — it flat-sums their desired heights and allocates them directly.
- **Typed scene, per-frame type erasure.** `LiveArea` stores `Scene` (concrete `TranscriptWidget`, `InputBarWidget`, `InfoBarWidget`, and an `Option<ScreenView>`). Each frame takes the scene out, erases the selected fields to `&mut dyn LiveAreaWidget` for the layout engine, and puts it back. There is no concrete type reification or long-lived boxed-widget owner.
- **Terminal scrollback promotion.** When the transcript FIFO exceeds its allocated rows, `TranscriptWidget::take_overflow()` drains the head and returns the overflow lines; `commit_scrollback()` renders them and calls `scroll_region_up` to push them into terminal scrollback. The pin-to-screen rows are the input and info bars only; while streaming, the top-level reasoning band and the transcript's embedded tool `StreamingWidget` slots are display-only and never reach scrollback. The FIFO is the sole overflow producer.
- **Single styled FIFO + top-level reasoning band.** The transcript is one styled FIFO and is the only render source for the transcript region. Prompt, response, tool-call, and system lines are pushed into it as styled lines as they arrive. Prompt and response text are markdown-rendered; committed tool blocks (`◆` title + tail) are inserted at the response boundary via the uniform [`commit_tool_stream_block`] path. Live tool output renders as `StreamingWidget` children **below** the FIFO — not embedded between bands within it. When a tool arrives and the response accumulator has accumulated text, that text is **sealed** into the FIFO before the tool widget is created, so model text that arrived before the tool call always appears before the tool's `◆` line. Live reasoning stays out of the FIFO as a top-level bottom band pinned directly above the input bar (a fixed-height, tail-anchored `StreamingWidget` that collapses to zero when idle). At `finish_turn`, the transcript first flushes any uncommitted tool-call entries and rebuilds the response, then the scene commits only the reasoning title line as the final FIFO line of the turn; the reasoning body is display-only and cleared with the live band. Finished history reads prompt → sealed text blocks → committed tool blocks → response → reasoning title. In-progress streams stay display-only and never enter the FIFO or terminal scrollback; only the committed tool tails and the final reasoning title become permanent transcript history.
- **Selected conversation, not a debug screen.** `Screen::Chat` shares one chat scene across `Target::Home`, `Target::StageExecution { run_id, stage }` selectors, and exact `Target::Invocation { run_id, stage, stage_run_id }` viewers. `LiveArea::select_conversation()` switches per-target render state; home keeps streaming in the background.
- **Watch is a screen, inline logs remain.** `Screen::Runs` / `RunsView` renders the watch table with an optional inline live-log pane. `Ctrl+W` enters watch and opens/closes that pane; `Ctrl+O` remains its inline-pane alias. Enter selects the row's conversation in chat. Watch content never commits to chat scrollback.
- **Uniform tool path.** Every tool — streaming or not — creates a `StreamingWidget` and commits via [`commit_tool_stream_block`]. Tools with an empty body commit a title-only block (functionally identical to the old diamond-line behavior); streaming tools commit their title plus the retained 5-row tail. `Read`/`Edit`/`Write`/`Glob`/`Grep` are no longer filtered — all tools participate uniformly. Live tool slots are `STREAM_MAX_ROWS`-capped, tail-anchored, lossy head-trimmed, ANSI-stripped, collapse to zero when idle, and never enter the transcript FIFO or terminal scrollback while streaming. The three-slice FIFO split (pre/events/tail) is gone; the FIFO renders as one contiguous tail-anchored block, with its rendered height tight to its content so live tool widgets sit directly beneath with no gap. **Sub-agent (`Task`) tool output is not forwarded:** nested loops run with `stream_events = None`, so their events never reach the parent TUI; `make_task_runner` therefore clears the inherited live-output sender (`child_ctx.tool_output = None`) so a sub-agent's chunks can't create a tool slot with no matching close event.
- **Home chat uses the persistent client.** Chat requests use `DaemonClient`; home events retain turn/stage_run_id identity and update `Target::Home` even while another conversation is selected.
- **Run follow is passive watch.** `follow_run()` / `follow_watch()` send `{"op":"watch","scope":"run",...,"replay":true}` on an auxiliary session socket. The strict `watched` ack must match scope, run id, replay, and both optional selectors (`stage` / `stage_run_id`, explicitly null when absent). Buffered and subsequent native frames are forwarded unchanged. Watch never attaches for control or sends pause/message commands.
- **Watch replay uses an atomic native cut, not content deduplication.** `watch_replay::replay_and_dedup()` collects at most 65,536 native snapshot frames through `replay_checkpoint`; `follow_run` has a bounded 256-frame socket queue. `history_gap` or an incomplete checkpoint triggers a one-shot persisted tail (500 lines), with a visible discontinuity/possible overlap warning, while preserving the same live subscription. Failed watch also uses disk-only persisted history, including after daemon restart; it never opens a replacement live log follow. Despite the helper name, this inline replay path performs no deduplication, sequence suppression, content matching, frozen anchor, occurrence budget, or replacement watch. Identical disk/native/live records remain visible. Replay completeness comes from `replay_checkpoint`, not an attach capability; native live frames carry emission sequences. Exec `.events` sidecars are ignored/not written and raw log metadata remains visible. Bounded disk tails assert no coverage; ordinary full log follow's persisted log-line cut remains separate. Agent stage history may include other executions; exact execution-scoped merging is deferred. See `docs/attachment-protocol.md` for journal limits and watermarks.
- **Log follow uses a dedicated socket.** The `log` op with `follow:true` monopolizes its connection, so it gets its own (`client::follow_log`).
- **Client owns transport; UI owns selection.** Watch Enter, `/attach`, and `/debug` select bus output without pausing execution or opening an input connection. Bus identity (`run_id`/`id`, `correlation_stage`/`stage`, `stage_run_id`) routes frames to exact invocation transcripts and matching run/stage selectors without requiring a start frame or nonce; per-target sequences suppress duplicate delivery using sorted merged inclusive ranges retained across close/reopen. `/debug <stage_run_id>` or `/debug <run> <stage_run_id>` resolves a concrete invocation; `/attach <run> <stage_run_id>` also selects one. Native input is separate and binds lazily on submission. Releasing local transports does not control execution.
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
- `Debug(selector)` → resolve the joined one- or two-argument concrete invocation selector in `mod.rs`, then select its bus conversation in `Screen::Chat`
- Selected execution controls are intercepted by `mod.rs` before ordinary dispatch; `commands.rs` does not resolve execution identities.

### Home chat flow
Session creation/recreation returns a registry-reserved `home_run_id`, distinct from
`sess-*`. The client installs it before processing watch rows, without waiting for
`TurnStarted`; first chat creation consumes the reservation. A replacement enqueue acknowledgment installs the authoritative run ID immediately,
without waiting for `TurnStarted`. Home follows that run through native bus replay/live
handoff; retained-window gaps remain visible. Unsolicited chat-display frames are ignored.
These steps apply to a new prompt on `Target::Home`; selected-stage text uses the correction path below.

1. User types plain text → Enter
2. `live_area.submit_prompt()` pushes a separator and the markdown-rendered prompt into the FIFO, then pins the turn start; the top-level reasoning band and the embedded tool slots are cleared with the new prompt
3. Force an immediate frame so the pinned prompt appears before the first daemon event
4. Set `app.active_request = true`, store `app.pending_user_message`
5. Spawn a task that sends `{"op": "chat", "text", "id"}` via the persistent `DaemonClient` (Esc sends `{"op": "cancel_chat"}`)
6. `EventKind::ReasoningChunk` → `live_area.push_reasoning_text()` (appends to the top-level reasoning band; partials are flushed on newline)
7. `EventKind::ToolOutputChunk` → `live_area.push_tool_output_chunk(id, call, text)` creates or updates the live stream keyed by call id (sealing any accumulated response text first); `EventKind::ToolResult` → `live_area.commit_tool_result(id, call)` commits that stream's title plus retained 5-row tail into the transcript via the uniform `commit_tool_stream_block` path. `EventKind::TurnComplete` seeds the turn's tool-call list for end-of-turn reconciliation; all tools participate uniformly
8. `EventKind::StreamChunk` → `live_area.push_response()` (accumulates raw response, marks the response region dirty)
9. `EventKind::Done` → reconcile the raw response accumulator with `Done.text` (use summary only if nothing streamed), then `live_area.finish_turn()` flushes any uncommitted tool-call list entries, rebuilds the response, unpins the turn, clears its draft state plus the live reasoning/tool slots, and commits the reasoning title as the final FIFO line of the finished turn (the reasoning body is discarded; committed tool blocks and the title stay in the transcript FIFO forever)
10. `Esc` during active request → abort chat task, send `cancel_chat`, `live_area.clear_current_turn()` truncates the FIFO back to the turn start

### Daemon events
`client::read_loop()` routes each main-socket frame exclusively to `event_rx`, `chat_rx`, a FIFO response waiter, or `raw_rx`; event classification ends with `continue`, so a boundary cannot also reach the raw path. The event loop serializes only `StageBoundary` before `observe_live_pipeline_frame()`; raw frames are observed directly without reserialization.

`DaemonEvent` variants arrive on `event_rx`:
- `RunStarted/Completed/Failed/Stopped` → update `app.active_runs`; terminal states also release a watch follow socket for the followed id
- `StageTransition` → update run stage
- `LogLine` → push to transcript if `app.following_log` matches (independent `/log <id>` following)
- `Bail` → log only

### Selected stage conversations
- Watch Enter and slash selectors observe bus output without pause-on-entry. Run/stage selectors and exact invocation viewers share the chat scene; exec stages remain read-only.
- Matching tagged agent events identify an exact agent execution and enable its composer. No debugger-ready frame or legacy attachment capability is required. A selector alone is not an input handle.
- Ordinary selected-agent text corrects that exact execution, never home chat or a replacement run. Native input binds lazily when a correction or control is submitted; queued prompts are busy, not input handles.
- Bus output routes by identity and rejects sibling invocations. Home bus events always update `Target::Home`, even while another target is displayed.
- Command receipts are separate from bus output. Track delivery against the submitted execution; stale receipts must not clear a successor execution's draft.
- `/pause` pauses; `/step` and `/continue` single-step one turn. Bare `/resume-execution` resumes the selected execution autonomously. Selected `/quit` also resumes; `/quit <reason>` and `/bail <reason>` bail with that reason.
- `/exit` and `/attach home` return home and release local input/selection without sending pause, resume, cancel, or other execution controls. Switching away does not cancel execution. Outside a selected execution, `/quit` exits the TUI.
Matching tagged agent events identify the exact input execution without requiring a
start frame or capability announcement. Queued prompts are busy, not input handles;
corrections use exact input and never fall back to chat. Command receipts are distinct
from bus output and cannot clear a successor execution's draft.

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

## Adding a New Keybinding

1. Add a `KeyCode` match arm in the `ct_rx` handler in `mod.rs` `run_app()`
2. Mirror the guard logic from existing chords: respect history search,
   selected-target/composer state, and Chat/Runs screen gates.
3. Add the chord to the keybindings section of `help_text()` in `commands.rs`
4. Update the keybindings table in `crates/gremlins/prompts/chat_system.md`
5. Update `DESIGN.md` if the binding changes screen-level behaviour

## Adding a New Chat Event

1. Add variant to `EventKind` in `gremlins-magic-bus/src/event_kind.rs` with a `#[serde(rename = "...")]` tag
2. Add the variant to `is_chat_event()` and `is_stream_event()` helpers
3. Add a match arm in the chat forwarding loop in `gremlins/src/executor/supervisor/chat/mod.rs`
4. Add a match arm in the `chat_rx` handler in `tui/mod.rs`

## Dependencies

- `ratatui` 0.30 with `crossterm` feature
- `crossterm` 0.29 for raw mode, events, cursor control, scroll region
- `pulldown-cmark` 0.13 for markdown rendering (tables, strikethrough, task lists)
- `tokio` for async runtime, channels, spawn
- `serde_json` for daemon protocol (JSON-line socket)
- `gremlins` (core crate) for config, executor types, socket helpers