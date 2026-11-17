# chat/ — TUI chat turn lifecycle

The chat sub-module handles the full lifecycle of a TUI chat turn: request
handling, gremlin creation/reuse, turn execution, cancellation, and cleanup.

## File map

| File | Role |
|---|---|
| `mod.rs` | Chat request handling and gremlin lifecycle: `handle_chat` (the op handler), gremlin lifecycle (`create_chat_gremlin`, `reusable_chat_gremlin_id`), and env resolution (`base_process_env`, `resolve_chat_env`). |
| `turn.rs` | Chat turn execution: `run_chat_turn` (the core turn loop), `handle_cancel_chat`, and the shared `TurnCtx` context. |
| `turn_finalizer.rs` | `TurnFinalizer`: owns the terminal-path resources for one turn. Consolidates re-insert-gremlin + clear-current-turn + signal-done + write-terminal-event into a single struct so every exit path doesn't repeat the same dance. `cancel` takes a `TurnEnd` (`Soft`/`Hard`/`Fallback`) that classifies the outcome. |
| `payload.rs` | Leaf helpers shared by the request handler and the turn loop: `chat_payload`, `usage_stats_from_state`, `add_usage_stats`. |

## Architecture

### Turn lifecycle

1. `handle_chat` receives the chat request, resolves the model, and either
   reuses the session's parked chat gremlin or creates a new one.
2. `handle_chat` builds a `TurnCtx`; `run_chat_turn` builds a `TurnFinalizer`
   and then enters the agent loop (delegating to `run_agent` from the executor).
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

`reusable_chat_gremlin_id` checks the model match; the gremlin is returned
to the session map by `TurnFinalizer::reinsert` after a turn.

### Cancellation

- **Cancel**: the caller (`handle_cancel_chat`) signals the cancel token;
  `run_chat_turn` catches the cancellation and calls `TurnFinalizer::cancel`
  with a `TurnEnd` (`Soft`/`Hard`/`Fallback`) classifying the outcome, which
  clears or preserves the per-turn artifact, re-inserts the gremlin, and
  emits `ended`.
- **Soft vs hard**: `TurnEnd::Soft` marks `ChatDefinition::interrupted` and
  preserves the per-turn artifact; `TurnEnd::Hard` clears it; and
  `TurnEnd::Fallback { reason }` handles the case where the run task finished
  without a terminal stream event.

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
- **Cancel is token-driven.** The agent loop checks the cancel token
  at iteration boundaries; it does not preempt an in-flight LLM call.

## Where to look for…

| You want to … | Look at |
|---|---|
| Change the turn loop | `turn.rs::run_chat_turn` + `TurnCtx` |
| Change cancellation behavior | `turn_finalizer.rs` — `cancel` + `TurnEnd`; `turn.rs::run_chat_turn` cancel arm |
| Change gremlin reuse policy | `mod.rs::reusable_chat_gremlin_id` |
| Change env resolution | `mod.rs::resolve_chat_env` |
| Change usage/cost tracking | `payload.rs::usage_stats_from_state`, `add_usage_stats` |
| Handle a new terminal path | `turn_finalizer.rs` — add a method to `TurnFinalizer` |
