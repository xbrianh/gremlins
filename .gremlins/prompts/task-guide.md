The Task tool delegates self-contained pieces of work to a worker.
Non-Fork kinds start with a clean conversation context; the Fork kind
inherits the parent conversation history.

Multiple Task calls issued in the same message execute concurrently
(up to 8). Use this to fan out independent subtasks and collect
results in a single round-trip.

Three `kind` variants:

- `Scout` — read-only exploration. Use for gathering information,
  reading files, or verifying assumptions without side effects.
- `Implement` — read/write work. Use for making code changes.
- `Fork` — inherits the parent conversation history. Use when the
  worker needs full context from earlier turns.
