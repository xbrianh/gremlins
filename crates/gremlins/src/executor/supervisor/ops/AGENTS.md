# ops/ — socket op handlers

The ops sub-module contains handlers for every opcode that can arrive on
the daemon's Unix socket. Handlers are grouped by category: lifecycle
mutations, queries, and model configuration.

`mod.rs` re-exports all handlers and also contains `handle_create_session`,
the coordinating factory that creates a new TUI session.

## File map

| File | Role |
|---|---|
| `mod.rs` | Re-exports all handlers from sub-modules. Contains `handle_create_session`: binds a per-session Unix socket, registers a `ClientSession` in the session map, and spawns the session listener. |
| `control.rs` | Lifecycle-mutating ops: `handle_launch`, `handle_stop`, `handle_resume`, `handle_rm`, `handle_session_stop`, `stop_session_chat_gremlin`. Also launch helpers: `parse_stage_inputs`, `resolve_filepath_source_values`, `validate_launch_inputs`, `load_definition_snapshot`, and `parallel_runtime_keys`. |
| `query.rs` | Read-only state inspection ops: `handle_ls`, `handle_status`, `handle_info`, `handle_artifact` (read + write path), `handle_snapshot`. Also `resolve_store_for_id` (resolve a state store from live run map or disk) and `extract_token_cost_fields`. |
| `model.rs` | TUI model configuration ops: `handle_model` (show/set/clear the TUI chat model override), `handle_chat_model` (read the effective chat model for a session), `handle_compact` (compact conversation history). |

## Architecture

### Op dispatch

`supervisor/mod.rs::dispatch_op` routes by the JSON `"op"` field. Each
handler receives the request JSON, the state root path, and optionally a
socket reader/writer (for streaming ops like `log`, `debug`). Handlers
return a JSON response value; streaming handlers write directly to the
shared writer.

### Launch flow

`handle_launch` (in `control.rs`):
1. Validates the request (`validate_launch_inputs`).
2. Resolves filepath source values relative to the project root
   (`resolve_filepath_source_values`).
3. Loads the definition snapshot (`load_definition_snapshot`).
4. Initializes a `Gremlin` and wires up channels via `spawn_gremlin_runner`.
5. Registers the run in `RUN_MAP` and emits a lifecycle event.

### Resume flow

`handle_resume` reconstructs a `Gremlin` from persisted state, collects
parallel runtime keys (`parallel_runtime_keys`), and re-spawns the runner.

### Query path

All query ops (`handle_ls`, `handle_status`, `handle_info`, `handle_artifact`,
`handle_snapshot`) use `state_view_for_id` / `resolve_store_for_id` to
present a unified view across live and persisted gremlins.

## Key invariants

- **Controls mutate; queries inspect.** Control ops change `RUN_MAP` and
  gremlin state. Query ops are read-only (except `handle_artifact`'s write
  path, which is gated to live gremlins only).
- **`handle_session_stop` stops both session-owned and run-map gremlins.**
  It first cancels any in-flight turn in the session, then delegates to
  `handle_stop` for the run-map path.
- **`parallel_runtime_keys` handles resume correctly.** It classifies
  markers relative to the target stage so completed children are recognized
  after resume.
- **Model ops are session-scoped.** `handle_model` sets a global TUI config
  override; `handle_chat_model` reads the effective model for a specific
  session's parked gremlin.

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new op handler | Add the function in the appropriate sub-module, re-export via `mod.rs`, wire in `supervisor/mod.rs::dispatch_op`. |
| Change launch behavior / inputs | `control.rs::handle_launch`, `parse_stage_inputs`, `validate_launch_inputs` |
| Change stop behavior | `control.rs::handle_stop`, `stop_session_chat_gremlin` |
| Change resume behavior | `control.rs::handle_resume`, `parallel_runtime_keys` |
| Change ls/status/info output fields | `query.rs::handle_ls` / `handle_status` / `handle_info` |
| Change model resolution for TUI | `model.rs::handle_model`, `handle_chat_model` |
| Change compaction logic | `model.rs::handle_compact` |
| Change artifact read/write | `query.rs::handle_artifact` |