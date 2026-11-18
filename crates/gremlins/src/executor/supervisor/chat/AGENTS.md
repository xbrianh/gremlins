# chat/ — TUI chat turn lifecycle

The chat sub-module handles the full lifecycle of TUI chat turns: request
handling, gremlin creation/reuse, turn execution, cancellation, and cleanup.
Each session's chat gremlin is driven by one persistent task that consumes a
bounded(1) prompt queue.

## File map

| File | Role |
|---|---|
| `mod.rs` | Chat request handling and gremlin lifecycle: `handle_chat` (the op handler), gremlin lifecycle (`create_chat_gremlin`, `reusable_chat_gremlin_id`, `spawn_fresh_chat_gremlin`), and env resolution (`base_process_env`, `resolve_chat_env`). |
| `turn.rs` | Chat turn execution: the persistent `chat_gremlin_task` / `run_turn` loop, the shared `TurnRunner` terminal-path helper, and `handle_cancel_chat`. |
| `../chat_handle.rs` | State-layer-neutral type shared by the actor and the turn loop: `ChatGremlinHandle` (prompt channel + persistent cancel token + finished flag). |
| `payload.rs` | Leaf module shared by the request handler and the turn loop: the per-turn `PromptMsg` queue item plus the `chat_payload`, `usage_stats_from_state`, `add_usage_stats` helpers. |

## Architecture

### Persistent task model

A session's chat gremlin is owned by a single long-lived task
(`turn::spawn_chat_gremlin` → `chat_gremlin_task`). The session stores a
`ChatGremlinHandle { id, prompt_tx, cancel, finished }` on its actor-owned
`SessionState`:

- `handle_chat` builds a `PromptMsg` (raw text, repo context, model, system
  prompt, project root, env, requested + discovered skills, and the connection
  writer for this turn) and sends it onto the handle's bounded(1) `prompt_tx`.
  A second prompt naturally blocks behind the in-flight turn, so turns
  serialize with no extra locking.
- `chat_gremlin_task` loops one `PromptMsg` at a time (racing the persistent
  cancel token). Between turns the gremlin is *parked* in the session; `run_turn`
  takes it out at turn start and the `TurnRunner` re-parks it on every exit path.

### Turn lifecycle

1. `handle_chat` resolves the model, discovers skills, and either reuses the
   session's parked gremlin (matching id + live handle) or calls
   `spawn_fresh_chat_gremlin` to reserve an id, build the gremlin, and start a
   task.
2. `handle_chat` queues a `PromptMsg`; the task takes the gremlin out of the
   session, installs a **fresh per-turn child `CancelToken`**, and runs exactly
   one `next_stage` → `run_stage` loop, streaming bus events to the connection.
3. When the turn completes, cancels, or errors, `TurnRunner` runs the terminal
   path exactly once: re-park the gremlin, restore the run-map token to the
   persistent one, clear `current_turn`, signal `done`, and write the terminal
   event (`done` / `ended` / `error`).

### Chat gremlin reuse

To avoid paying definition-load and client-setup costs on every TUI message,
the gremlin is parked in the session between turns. Reuse conditions:
- The parked gremlin's id matches the handle's id.
- The handle's task is still alive (`!is_finished()`).

The per-turn model is rebuilt from the `PromptMsg` each turn, so a `/model`
change takes effect on the next turn without recreating the gremlin.

### Cancellation

Two cancel tokens govern a chat gremlin:

- A **persistent** token (`ChatGremlinHandle::cancel`) stops the task for good.
  `handle_cancel_chat` in `"permanent"` mode cancels both the in-flight child
  token and the persistent one, so the current turn aborts rather than drains;
  the loop exits and later prompt sends fail, which `handle_chat` detects.
  `ChatGremlinHandle::finished` (an `AtomicBool` set by the task on exit) is the
  authoritative liveness signal.
- A per-turn **child** token is installed on the run map for each turn (and held
  by `CurrentTurn`). Soft/hard cancel modes cancel that child token, which the
  turn's event loop races; soft marks `ChatDefinition::interrupted` and preserves
  the per-turn artifact, hard clears both. If a debugger was attached, the
  runner gives it the same terminal reason after the stage unwinds.

### Env resolution

`resolve_chat_env` prefers the TUI-supplied `env` object from the request.
If absent, it falls back to `BASE_PROCESS_ENV` — a snapshot of the daemon
process environment captured once at startup.

## Key invariants

- **A gremlin is in exactly one place at a time.** While a turn runs, the
  gremlin is removed from the session and held by `run_turn`. After the turn,
  `TurnRunner` re-parks it.
- **`TurnRunner` owns the terminal path.** Every exit from `run_turn` must go
  through a `TurnRunner` method — don't re-insert, restore the run-map token, or
  signal done by hand.
- **The persistent task is the only driver.** The gremlin is never driven
  outside `chat_gremlin_task`; `handle_chat` only queues prompts.
- **Cancel is token-driven.** The active turn's token interrupts model streams,
  tool batches, and debug command waits. Debugger disconnect resumes the run;
  it does not cancel the chat turn.

## Where to look for…

| You want to … | Look at |
|---|---|
| Change the turn loop | `turn.rs::run_turn` + `ChatGremlinHandle` / `PromptMsg` |
| Change the persistent task | `turn.rs::chat_gremlin_task` |
| Change the handle/queue types | `../chat_handle.rs` |
| Change cancellation behavior | `turn.rs::handle_cancel_chat`; `TurnRunner` cancel/terminal methods |
| Change gremlin reuse policy | `mod.rs::reusable_chat_gremlin_id`, `spawn_fresh_chat_gremlin` |
| Change env resolution | `mod.rs::resolve_chat_env` |
| Change usage/cost tracking | `payload.rs::usage_stats_from_state`, `add_usage_stats` |
| Handle a new terminal path | add a method to `turn.rs::TurnRunner` |
