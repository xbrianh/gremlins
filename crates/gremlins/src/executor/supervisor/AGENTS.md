# supervisor/ — the daemon layer

The supervisor is the single-instance daemon that owns the global run map,
the socket accept loop, and all op dispatch. It runs as `gremlins serve` and
is spawned by the CLI for every command that needs a daemon (`launch`, `stop`,
`resume`, `ls`, etc.).

The module was split from a monolithic `supervisor.rs` during the
`break-executor` refactor. `mod.rs` remains the hub: it holds `RunHandle`,
`RunState`, the accept loop, `dispatch_op`, shared helpers (`SharedWriter`,
`CurrentTurn`), and re-exports.

## File map

| File | Role |
|---|---|
| `mod.rs` | Hub: module declarations, `RunHandle`/`RunState`, `CurrentTurn`, `SharedWriter`, `DebugDetachGuard`, accept loop (`run_supervisor`, `handle_connection`), `dispatch_op`, `ok_response`/`error_response`, and all pub(crate) re-exports. |
| `event.rs` | Global event bus (`Bus<EventKind>`), `init_bus`, `subscribe_all`/`subscribe_run`, `emit_event`, connection counter, `ConnectionGuard`, and `check_shutdown`. |
| `lifecycle.rs` | Child gremlin lifecycle: `setup_gremlin_channels`, `run_gremlin_task`, `spawn_log_writer`, `spawn_gremlin_runner`, `launch_child`/`stop_child`, `terminate_and_reap`. |
| `log_handler.rs` | Streaming log handler (`handle_log`) and `LogSource` enum. |
| `debug.rs` | Interactive debug handler (`handle_debug`) and `send_debug_status`. |
| `session.rs` | Client session lifecycle: `ClientSession` struct, `SessionOp` enum, `run_session_listener`/`run_session_dispatcher`, `handle_session_main`/`handle_session_op`, session map, and `teardown_session`. |
| `state_view.rs` | `StateView` — a uniform JSON snapshot of a gremlin's state across live (run map) and persisted (disk) sources. Used by the read-only query ops. |
| `chat/` | TUI chat turn lifecycle. See `chat/AGENTS.md`. |
| `ops/` | Socket op handlers. See `ops/AGENTS.md`. |
| `tests.rs` | Integration tests for supervisor internals: channel losslessness, chat gremlin reuse, model resolution, artifact reads, env fallback, compaction, etc. |

## Architecture

### Daemon lifecycle

1. `gremlins serve` calls `run_supervisor`, which binds the Unix-domain
   socket, initializes the event bus and connection counter, and enters the
   accept loop.
2. Each connection spawns `handle_connection`, which reads JSON-line
   requests, dispatches via `dispatch_op`, and writes JSON-line responses.
3. Session-bearing connections (`handle_session_main`) additionally spawn
   the session listener and dispatcher for sideline sockets (log, debug,
   chat).

### Run map

The global `RUN_MAP` (`HashMap<String, RunHandle>`) is the single source of
truth for running gremlins. Each `RunHandle` carries:
- A cancel token and abort flag
- The tokio task handle
- A `watch::Sender<RunState>` for status broadcasts
- A log `broadcast::Sender<String>` for streaming logs
- An `InteractiveHandle` for debug/command injection
- The gremlin's `StateStore`
- A bus `Subscriber<EventKind>` for lifecycle events

`RunHandle::finish` removes the entry, publishes the terminal `RunState`,
and emits the terminal `DaemonEvent`.

### Op dispatch

`dispatch_op` routes by the JSON `"op"` field:

| Op | Handler | Module |
|---|---|---|
| `launch` | `handle_launch` | `ops/control.rs` |
| `stop` | `handle_stop` | `ops/control.rs` |
| `resume` | `handle_resume` | `ops/control.rs` |
| `rm` | `handle_rm` | `ops/control.rs` |
| `ls` | `handle_ls` | `ops/query.rs` |
| `status` | `handle_status` | `ops/query.rs` |
| `info` | `handle_info` | `ops/query.rs` |
| `artifact` | `handle_artifact` | `ops/query.rs` |
| `snapshot` | `handle_snapshot` | `ops/query.rs` |
| `model` | `handle_model` | `ops/model.rs` |
| `chat_model` | `handle_chat_model` | `ops/model.rs` |
| `compact` | `handle_compact` | `ops/model.rs` |
| `log` | `handle_log` | `log_handler.rs` |
| `debug` | `handle_debug` | `debug.rs` |
| `chat` | `handle_chat` | `chat/mod.rs` |
| `create_session` | `handle_create_session` | `ops/mod.rs` |
| `session_stop` | `handle_session_stop` | `ops/control.rs` |

## Key invariants

- **`RUN_MAP` is the only authority on liveness.** A gremlin is "live" iff
  it has an entry in the map. `StateView` checks the map first, then falls
  back to persisted state.
- **`RunHandle::finish` must always be called.** Every exit path — success,
  bail, error — must call `finish` so the map entry is cleaned, the terminal
  state is broadcast, and bus subscribers are notified.
- **Session sockets are per-session.** Each TUI session gets a dedicated
  Unix socket (`<session_id>.sock`) for sideline channels (log, debug,
  chat). The session id uses a `sess-` prefix to never collide with gremlin
  ids.
- **Chat gremlins are session-scoped.** A `ClientSession` owns its chat
  gremlins; they are parked in `chat_gremlins` between turns. A turn
  removes its gremlin while running and re-inserts it when done.
- **The event bus is global.** `init_bus` creates a single `Bus<EventKind>`
  on first access. Subscribers filter by run id via `subscribe_run`.

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new op handler | Add a function in the appropriate `ops/` sub-module, wire it in `dispatch_op` (`mod.rs`). |
| Change child launch/stop | `lifecycle.rs::launch_child` / `stop_child` |
| Change the event bus | `event.rs::init_bus`, `subscribe_run`, `emit_event` |
| Understand session lifecycle | `session.rs::ClientSession`, `run_session_listener`, `teardown_session` |
| Debug a chat turn | `chat/mod.rs::run_chat_turn` + `chat/turn_finalizer.rs` |
| Understand the interactive debug flow | `debug.rs::handle_debug` |
| Read gremlin state for a query | `state_view.rs::state_view_for_id` |
| Change socket/lock protocol | `supervisor` delegates to `executor/socket.rs` |