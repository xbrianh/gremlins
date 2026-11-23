# supervisor/ — the daemon layer

The supervisor is the single-instance daemon that owns the global run map,
the socket accept loop, and all op dispatch. It runs as `gremlins serve` and
is spawned by the CLI for every command that needs a daemon (`launch`, `stop`,
`resume`, `ls`, etc.).

The module was split from a monolithic `supervisor.rs` during the
`break-executor` refactor. `mod.rs` remains the hub: it holds the accept loop,
`dispatch_op`, shared helpers (`SharedWriter`, `ok_response`/`error_response`),
and re-exports. The `RunHandle`/`RunState` types live in `run_registry.rs`, and
`CurrentTurn` in `chat/mod.rs`.

## File map

| File | Role |
|---|---|
| `mod.rs` | Hub: module declarations, accept loop (`run_supervisor`, `handle_connection`), `dispatch_op`, `ok_response`/`error_response`, and all pub(crate) re-exports. |
| (bus crate) | The global event bus (`Bus<EventKind>`), `init_bus`, `subscribe_all`/`subscribe_run`, `emit_event`, `SharedWriter`, `BusSubGuard`, and `ConnectionGuard` live in the `gremlins-magic-bus` crate (`concrete.rs`, `connection.rs`). |
| `lifecycle.rs` | Child gremlin lifecycle: `setup_gremlin_channels`, `run_gremlin_task`, `spawn_log_writer`, `reap_client_for`, `terminate_and_reap`, and `LogRelay`. |
| `run_registry.rs` | `RunHandle`/`RunState`/`LaunchResult` types plus the lock-protected `RunRegistry` owning the run map, connection counter, and chat-id sequence. |
| `log_handler.rs` | Streaming log handler (`handle_log`) and `LogSource` enum. |
| `attachment.rs` | Attachment handler (`handle_attach`): `run`-scope stage-boundary follow and `conversation`-scope observe/control. The only op that pins a pause and injects operator text. |
| `debug.rs` | Shared attachment helpers: `check_agent_stage` (agent-leaf validation) and `send_debug_status` (progress frames). |
| `session.rs` | Client session IO layer: `SessionOp` enum, `run_session_listener`/`run_session_dispatcher`, `handle_session_main`/`handle_session_op`/`handle_session_aux`, and `verify_session_id`. Uses `SessionTaskHandle` for session state and `RunRegistry` for run operations. |
| `session_task.rs` | Per-session state owner: `SessionState`, `SessionTaskMsg`, `SessionTaskHandle`, `spawn_session_task`/`run_session_task`, and teardown. Owns parked chat gremlins, current turn, reserved home run id, listener and auxiliary tasks. |
| `prompt_msg.rs` | `PromptMsg` shared by the session and chat turn loop. |
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
   the session listener and dispatcher for sideline sockets (log, attach,
   chat).

### Run map

The shared `RunRegistry` owns a lock-protected `HashMap<String, RunHandle>`
and is the single source of truth for running gremlins. It also owns the
connection counter, per-id locks, chat-id sequence, and pending-output scopes.
Each `RunHandle` carries:
- A cancel token and abort flag
- The tokio task handle
- A `watch::Sender<RunState>` for status broadcasts
- A log flush handle for persisted-history handoff
- An `InteractiveHandle` for attach/command injection
- The gremlin's `StateStore`
- A bus `Subscriber<EventKind>` for the lossless log writer

`RunRegistry::handle_run_finished` removes the entry, publishes the terminal
`RunState`, emits the terminal bus event, and releases the log subscription.
If a stop already removed the entry, completion is a no-op; stop paths own
that terminal publication and cleanup.

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
| `attach` | `handle_attach` | `attachment.rs` |
| `chat` | `handle_chat` | `chat/mod.rs` |
| `create_session` | `handle_create_session` | `ops/mod.rs` |
| `session_stop` | `handle_session_stop` | `ops/control.rs` |

### Conservative run replay

- Run-scope `attach` opts into history with `"replay": true`. Its `attached`
  acknowledgment advertises `replay_checkpoint` alongside `observe` only when
  replay is negotiated. Without that capability, do not assume a checkpoint.
- Replay atomically snapshots the bounded in-memory bus journal and registers
  live delivery. It sends `history_gap` when history is incomplete, retained
  display frames with their original `sequence`, then `replay_checkpoint`
  (`complete`, `through_sequence`, `evicted_through_sequence`) before live frames.
  Live run display frames carry `sequence` even without replay negotiation.
- Completeness is conservative and bus-global: any eviction or retention omission
  makes it false, even for unrelated run filters. This is an in-memory cut, not
  proof that a disk tail covers live output. Lag-tolerant delivery can still lag.
- Session `read_blob` preserves raw log/agent/exec lines and metadata, returning
  `through_sequence: null` and `covered_sequences: null`. Exec `.events` sidecars
  are neither written nor trusted: process-local sequence values can collide
  across restarts. Empty/missing tails also assert no coverage.
- Ordinary full operational `log` follow retains its separate log-writer
  persistence cut: the serialized writer snapshots persisted content and its
  sequence for history/live suppression. Bounded tails use zero coverage.
- Bus journal insertion, snapshotting, and subscription registration are atomic
  under the bus mutex; persistence/read callbacks run outside it. No disk I/O
  under that lock, and no atomic publication/persistence guarantee. Compatibility
  bus read helpers return zero rather than asserting a persisted replay cut.
- Exact execution-scoped persisted agent history/live merging remains deferred;
  neither correlation metadata nor replay checkpoints establish that guarantee.

## Key invariants

- **`RunRegistry` is the only authority on liveness.** A gremlin is "live" iff
  it has an entry in the map. `StateView` checks the map first, then falls
  back to persisted state.
- **Run completion must reach registry cleanup.** Every normal task exit
  reaches `RunRegistry::handle_run_finished`; stop paths remove and reap
  the run themselves. Both paths publish terminal state and release log
  subscriptions.
- **Session sockets are per-session.** Each TUI session gets a dedicated
  Unix socket (`<session_id>.sock`) for sideline channels (log, attach,
  chat). The session id uses a `sess-` prefix to never collide with gremlin
  ids.
- **Chat gremlins are session-scoped.** A session owns its chat
  gremlins (in `session_task.rs::SessionState`); they are parked in
  `chat_gremlins` between turns. A turn
  removes its gremlin while running and re-inserts it when done.
- **The event bus is global.** `init_bus` creates a single `Bus<EventKind>`
  on first access. Subscribers filter by run id via `subscribe_run`.

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new op handler | Add a function in the appropriate `ops/` sub-module, wire it in `dispatch_op` (`mod.rs`). |
| Change child launch/stop | `run_registry.rs::RunRegistry::launch_child` / `stop_and_reap` |
| Change the event bus | `gremlins_magic_bus::concrete` (`init_bus`, `subscribe_run`, `emit_event`) |
| Understand session lifecycle | `session.rs::run_session_listener` / `handle_session_main`; state in `session_task.rs::SessionState`, accessed through `SessionTaskHandle` |
| Debug a chat turn | `chat/turn.rs::run_turn` + `chat_handle.rs::ChatGremlinHandle` / `prompt_msg.rs::PromptMsg` |
| Understand the interactive attach flow | `attachment.rs::handle_conversation_attach` |
| Read gremlin state for a query | `state_view.rs::state_view_for_id` |
| Change socket/lock protocol | `supervisor` delegates to `crate::executor::socket` |