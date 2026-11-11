# agent — Agent loop subsystem

The core turn-by-turn agent orchestration: stream a model completion, execute any tool calls, fold the result into conversation history, and either loop for another turn or signal completion via the `Done` tool. This is the heart of every agent stage, every Task child, and every Fork clone.

## Module map

```
agent_loop.rs       Main orchestrator: run_agent_loop / run_agent_loop_core
                    + LoopState, StreamAccumulator, ToolJob, retry/backoff
stream_consume.rs   Drive the SSE/JSON-lines stream to completion
                    (consume_stream_events, finish_stream_turn)
turn_flow.rs        Post-turn resolution: handle_done_decision,
                    handle_empty_turn, commit_turn_to_history,
                    fold_turn_usage, emit_turn_artifacts
done_helpers.rs     Build the Done action-ledger + tool_result user message
interactive.rs      Pause / interactive-session state machine
                    (maybe_pause, interactive_loop, await_interactive_turn)
plug.rs             Inversion-of-control hooks: log, prompt debug dump,
                    token-usage emit. Wired by executor/run.rs.
system_prompts.rs   Agent/task/fork system prompts + chat preamble helpers.
                    Also owns BATCHING_GUIDANCE.
task.rs             Task runner factory: make_task_runner, task id chains
task_models.rs      Per-TaskKind model overrides (task-models config)
```

## Architecture

`run_agent_loop_core` (agent_loop.rs) is the main while-loop:

```
LoopState::run_agent_loop_core
  ├─ LoopState::stream_turn               // one model completion turn
  │   ├─ prepare_stream_request            // build messages
  │   ├─ launch_model_stream               // error-handling + retry
  │   ├─ consume_stream_events             // async event loop (stream_consume.rs)
  │   └─ finish_stream_turn                // classify outcome (stream_consume.rs)
  └─ LoopState::execute_tools             // batch tool execution
      ├─ collect_tool_jobs                 // validate + build ToolJob list
      ├─ dispatch_tool_jobs                // concurrent execute with cancel
      └─ emit_tool_results                 // history + prompt roll-forward
```

After each tool batch, `turn_flow.rs` functions handle turn resolution:
- `handle_done_decision` — inspect tool calls for a `Done` signal
- `handle_empty_turn` — act on a turn with no tool calls
- `commit_turn_to_history` — append assistant turn to conversation

## Key types

- **`LoopState`** — mutable state carried through each turn: conversation handle, usage accumulators, turn counter, raw/captured writers, timeout backoff schedule, hooks.
- **`LoopOpts`** — immutable per-run options: `tool_filter`, `temperature`, `max_tokens`, `extra`, `tool_mode`, `preserve_reasoning_in_history`.
- **`RunContext`** — everything the caller (backend/executor) wires in: `RunParams`, sandbox roots, prefix, timeout, artifact expectation budget, hooks.
- **`ToolJob`** — a single tool call queued for execution: `ToolCall` + `CallId` + `ToolName`. Dispatched concurrently with cancellation racing.
- **`StreamAccumulator`** / **`StreamTurnOutput`** / **`StreamTurnOutcome`** — the stream consumption result: text, tool calls, reasoning, usage, and the control-flow signal (Continue, Done, Empty, Bail, Quit).

## Task subsystem

`task.rs` builds the `TaskFn` closure wired into `AgentLoopRuntime.task_fn` before the turn loop starts. Each Task invocation:

1. Checks depth vs `MAX_DEPTH` (3)
2. Optionally swaps the model via `TaskModelSelector` (per-kind overrides from `task-models` config)
3. Builds a child loop via `run_agent_loop_nested`
4. Returns the child's text result as a `ToolOutcome`

Task id chains (`[task.a3f1.b72e]`) are monotonic 16-bit hex segments. Siblings share one depth level — only genuine nesting increments the bound.

## Conventions

- **Free functions over methods.** `stream_consume.rs` and `turn_flow.rs` are free functions that accept `&mut LoopState`. They access `LoopState` fields through `pub(super)` visibility. This keeps concerns in separate files without splitting `LoopState` methods across them.
- **Cancellation awareness.** Every async point races against `CancelToken`. `send_stream_event_checked` propagates cancellation via `?` so callers unwind cleanly.
- **Retry.** `retry_backoff` sleeps with cancellation awareness. The inner loop retries 429s/transients at the turn boundary before exhausting.
- **No stderr for agent talk.** All agent prose goes through the `log_fn` hook (which writes to the per-gremlin log channel), never raw stderr. `write_raw` is for raw transcript files.