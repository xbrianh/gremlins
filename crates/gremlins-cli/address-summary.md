# Fix Summary

## Critical correctness fixes

### `crates/gremlins/src/clients/agent/agent_loop.rs`, line ~1149
**Category**: Correctness
**Issue**: `run_agent_loop` hardcoded `wants_events: false` in its call to `run_agent_loop_core`, suppressing all `StreamChunk`, `ReasoningChunk`, and `TurnComplete` events for the top-level interactive agent loop.
**Fix**: Compute `wants_events` from `interactive.is_some()` before the `interactive` value is moved into `run_agent_loop_core`. The top-level entry point now passes `wants_events: true` when an interactive session is present, restoring live text/reasoning streaming and turn-completion broadcasts for the chat TUI.

### `crates/gremlins/src/clients/agent/stream_consume.rs`, lines ~183, ~201, ~223 and `turn_flow.rs`, line ~434
**Category**: Correctness
**Issue**: The `chain` field in every `EventKind::TaskProgress` emission was hard-coded to `String::new()`, carrying no data despite the protocol carrying the field.
**Fix**: Threaded `chain: &str` through the full parameter cascade — `run_agent_loop_core`, `LoopState::stream_turn`, `LoopState::consume_stream_events`, the free `consume_stream_events`, `LoopState::emit_turn_artifacts`, and the free `emit_turn_artifacts` — so the actual chain id from `run_agent_loop_nested` (via `id_chain`) reaches all four emission sites. The top-level `run_agent_loop` passes `""` for chain.

### `crates/gremlins/src/clients/tools/handlers/task.rs`, line 27 and 67
**Category**: Readability
**Issue**: The function parameter was named `_id` (underscore-prefixed, Rust convention for intentionally unused) but was now consumed to build `parent_id`.
**Fix**: Renamed `_id` to `id` at the binding site and the use site.

## Dead code removal

### `crates/gremlins/src/clients/agent/task.rs`, lines ~76-77 and ~115-116
**Category**: Architecture
**Issue**: `make_task_runner` and `make_task_runner_at_depth` accepted `parent_id: String` and `parent_call: String` parameters that were never used — the values always reach the nested loop via the extended `TaskFn` closure callback instead.
**Fix**: Removed the dead parameters from both function signatures, the recursive call site in `make_task_runner_at_depth`, and the call site in `run_agent_loop`.

### `crates/gremlins-cli/src/tui/chat.rs`, lines ~72 and ~223
**Category**: Correctness
**Issue**: `ChatEvent::TaskProgress` carried a `turn: Option<String>` field that was never populated (no `turn` member exists on `EventKind::TaskProgress` in `gremlins-magic-bus`).
**Fix**: Removed the `turn` field from the `TaskProgress` variant and its parsing constructor. The handler in `tui/mod.rs` was restructured to process `TaskProgress` before the turn-staleness filter (which is turn-keyed), since task progress is keyed by parent tool-call id, not turn.

## Title rendering

### `crates/gremlins-cli/src/tui/screen.rs`, line 570 and `crates/gremlins-cli/src/tui/widgets.rs`, lines ~290-326
**Category**: Public API / Readability
**Issue**: `LiveArea::push_task_progress` accepted `kind` but dropped it (`_kind`), never forwarding it to the widget. The task kind never appeared in the slot title directly.
**Fix**: Forwarded `kind` through both `LiveArea::push_task_progress` and `TranscriptWidget::push_task_progress`. The title now renders as `"{kind}: {call}  {stats}"` when both kind and call are present, `"{kind}"` when call is empty, and `"{call}"` when kind is empty — ensuring the task type is always visible in the slot title.

## Test compilation fixes

### `crates/gremlins/src/clients/agent/agent_loop.rs` and `crates/gremlins/src/clients/agent/task.rs`
**Category**: Testing
**Issue**: Mechanical compilation failures in existing tests due to signature changes (new `chain` parameter on `run_agent_loop_core`, removed `parent_id`/`parent_call` from `make_task_runner`).
**Fix**: Added `""` for `chain` at every test call to `run_agent_loop_core`. Removed trailing `String::new(), String::new(),` args from every test call to `make_task_runner`.