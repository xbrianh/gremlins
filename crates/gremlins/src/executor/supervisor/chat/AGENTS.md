# chat/ — TUI chat turn lifecycle

The chat sub-module handles the full lifecycle of a TUI chat turn: request
handling, gremlin creation/reuse, turn execution, cancellation, and cleanup.

## File map

| File | Role |
|---|---|
| `mod.rs` | Chat turn entry point: `handle_chat` (the op handler), `run_chat_turn` (the core turn loop), `chat_payload`, gremlin lifecycle (`create_chat_gremlin`, `register_chat_gremlin`, `cleanup_chat_gremlin`, `reusable_chat_gremlin_id`, `reinsert_chat_gremlin`), env resolution (`base_process_env`, `resolve_chat_env`), and usage accounting (`usage_stats_from_state`, `add_usage_stats`). |
| `turn_finalizer.rs` | `TurnFinalizer`: owns the terminal-path resources for one turn. Consolidates re-insert-gremlin + clear-current-turn + signal-done + write-terminal-event into a single struct so every exit path doesn't repeat the same dance. Also handles `hard_cancel`, `soft_interrupt`, and `task_fallback`. |

## Architecture

### Turn lifecycle

1. `handle_chat` receives the chat request, resolves the model, and either
   reuses the session's parked chat gremlin or creates a new one.
2. `run_chat_turn` builds a `TurnCtx` and a `TurnFinalizer`, then enters
   the agent loop (delegating to `run_agent` from the executor).
3. On each agent turn, the stage stream emits events and the TUI receives
   incremental updates.
4. When the turn completes, cancels, or errors, `TurnFinalizer` handles
   all cleanup paths: re-insert the gremlin into the session, clear the
   in-flight turn record, signal `done` on the watch channel, and write
   the terminal event to the stream.

### Chat gremlin reuse

To avoid paying definition-load and client-setup costs on every TUI
message, the chat handler parks the gremlin in the session's
`chat_gremlins` map between turns. Reuse conditions:
- The parked gremlin's model matches the requested model (if the model
  changed, the client must be rebuilt).
- The gremlin is not currently running a turn.

`reusable_chat_gremlin_id` checks the model match; `reinsert_chat_gremlin`
returns the gremlin to the session map after a turn.

### Cancellation

- **Soft cancel** (`soft_interrupt`): sets `soft_cancel` on the turn,
  which the agent loop checks between iterations.
- **Hard cancel** (`hard_cancel`): aborts the run task via the cancel
  token, reaps the gremlin, and signals done immediately.
- `TurnFinalizer::task_fallback` handles the case where the run task is
  already gone when cancellation arrives.

### Env resolution

`resolve_chat_env` prefers the TUI-supplied `env` object from the request.
If absent, it falls back to `BASE_PROCESS_ENV` — a snapshot of the daemon
process environment captured once at startup.

## Key invariants

- **A gremlin is in exactly one place at a time.** While a turn runs, the
  gremlin is removed from `chat_gremlins` and held by the turn task. After
  the turn, it is re-inserted.
- **`TurnFinalizer` owns the terminal path.** Every exit from
  `run_chat_turn` must go through the finalizer — don't re-insert or signal
  done by hand.
- **Model changes force client rebuild.** If the TUI's chat model changes,
  the parked gremlin is invalidated and a new one is created.
- **Soft cancel is advisory.** The agent loop checks `soft_cancel` at
  iteration boundaries; it does not preempt an in-flight LLM call.

## Where to look for…

| You want to … | Look at |
|---|---|
| Change the turn loop | `mod.rs::run_chat_turn` + `TurnCtx` |
| Change cancellation behavior | `turn_finalizer.rs` — `soft_interrupt`, `hard_cancel`, `task_fallback` |
| Change gremlin reuse policy | `mod.rs::reusable_chat_gremlin_id`, `reinsert_chat_gremlin` |
| Change env resolution | `mod.rs::resolve_chat_env` |
| Change usage/cost tracking | `mod.rs::usage_stats_from_state`, `add_usage_stats` |
| Handle a new terminal path | `turn_finalizer.rs` — add a method to `TurnFinalizer` |