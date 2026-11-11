# clients/tools/ — agent tool invocation framework

The tools module is the dispatch and containment layer for every agent-side
tool call. It owns gate/validate/audit/dispatch, the `ToolHandler` trait,
the handler registry, and the per-tool sandbox enforcement. The module is
structured so that the runtime (AgentLoopRuntime, ToolOutcome, sandbox
mode) is cleanly separated from dispatch policy (filtering, capping,
auditing) and from individual tool implementations (in `handlers/`).

## File map

| File | Role |
|---|---|
| `mod.rs` | Module declarations and crate-internal re-exports. Centralizes the public surface the agent loop and executor consume. |
| `defaults.rs` | Global `ToolRegistry` singleton (`OnceLock`), plus forwarding helpers (`max_per_message`, `display_truncation`, `is_completion`, `title_detail`, `title_uses_secondaries`, `secondary_args`) that look up a tool by name and delegate to its trait method. |
| `definitions.rs` | Produces model-facing `ToolDefinition` JSON schemas from `ToolSpec` tables. Handles tool filtering, chat-only omission, and the `{TASK_MAX_PER_TURN}` / `{MAX_CONCURRENT_TOOLS}` placeholder substitution. |
| `dispatch.rs` | The main tool-call pipeline: `invoke_with_conversation` is the single entry point. Sequence: chat-only gate → tool-filter gate → JSON parse → handler auto-repair → schema validation → sandbox check → handler invoke → audit write → output cap. |
| `registry.rs` | `ToolHandler` trait (the interface every handler implements) and `ToolRegistry` (name→handler map). Also contains presentation helpers (`str_detail`, `push_u64_arg`, `push_str_arg`) shared by handlers and the agent loop. |
| `runtime.rs` | `ToolOutcome` (prose text + optional structured exit code), `SandboxMode` (Roots vs AllowAll), `AgentLoopRuntime` (the per-stage context: cwd, env, sandbox, audit log, tool filter, task callback, telemetry channels, skills, id prefix, log channel), `AgentLoopTelemetry` (stream events + live tool-output sender), `ToolOutputLine` (one live-display chunk), and `TaskFn` (the callback type for spawning nested agent loops). |
| `sandbox.rs` | Path containment enforcement. `check_tool` delegates per-tool sandbox checks to each handler via `sandbox_check()`. `bash_check` does pre-flight static analysis of shell commands — it splits on `;`/`&&`/`||`, tracks simulated `cd` state, strips heredocs and subshells, and validates every path-looking token against canonical containment roots. Also contains the audit logging path (`audit_log_path`), the audit entry writer, and the `check_ln_symlink` defense-in-depth check. |
| `schema.rs` | Argument validation. `validate_tool_args` checks required fields and types against each tool's `ToolSpec` parameter schema, plus Edit-nested constraints (non-empty edits array, no legacy `replacement`, required `old_string`/`new_string`, integer scope fields). |
| `tool_specs.rs` | `ToolSpec` and `FieldSpec` tables — the single source of truth for every tool's name, description, parameter names/types/descriptions/requiredness, and per-field validation flags. Also `ToolMode` (Full vs NoTools), `MAX_CONCURRENT_TOOLS` (8), `TASK_MAX_PER_TURN` (8), and `always_available()` (Task + Done). |
| `util.rs` | Small helpers: `preview_str` (display escaping), `result_status` (ok/error classification for audit), `blocking_string` (spawn_blocking wrapper), `parse_args` (JSON parse with error message), `req_str` (required string field extraction), `truncate_to_bytes` (UTF-8-safe truncation). |
| `tests.rs` | Integration tests covering the full dispatch pipeline: tool filtering, sandbox enforcement, output capping, audit logging, argument validation, Bash streaming/timeout/sandbox, Grep/Glob context behavior, and the Edit batched-edit machinery. |

## Architecture

### Dispatch pipeline

Every tool call flows through `dispatch::invoke_with_conversation`:

1. **Chat-only gate** — chat-only tools (Mischief, WebSearch) are rejected in
   headless mode with "unknown tool" (so the model cannot detect mode via
   error message differences).
2. **Tool-filter gate** — when `allowed_tools` is set, only explicitly listed
   tools pass. `always_available` (Task, Done), `bypass_filter` (LoadSkill),
   and chat-only-in-chat bypass the filter.
3. **JSON parse** — parse args; invalid JSON returns an error and writes an
   audit entry.
4. **Auto-repair** — handlers with `repair_args()` get a chance to fix common
   argument malformations before validation (only Edit implements this).
5. **Schema validation** — `validate_tool_args` checks required fields and
   types against the `ToolSpec` parameter schema.
6. **Sandbox check** — `check_tool` delegates to each handler's
   `sandbox_check()`. For Bash, this runs the full `bash_check` static
   analysis pipeline (heredoc stripping, subshell stripping, command
   splitting, cd tracking, path-token validation).
7. **Handler invoke** — the handler's `invoke()` runs the tool. Fan-out tools
   (Task) receive the parent conversation for seeding child loops.
8. **Audit** — writes a JSONL entry with timestamp, tool name, key argument,
   and ok/error status.
9. **Output cap** — for tools with `output_cap() == true` (Read, Bash, Grep,
   Glob, Landscape, WebSearch), the output is truncated to
   `max_tool_output_bytes()` with a per-tool marker suffix if it exceeds the
   budget.

### ToolHandler trait

Every handler implements `ToolHandler` (in `registry.rs`). The trait
provides hooks for dispatch policy, presentation, and containment — keeping
tool-specific logic out of the dispatch layer:

- `invoke` — the tool's actual logic
- `max_per_message` — concurrency budget for fan-out tools (only Task)
- `display_truncation` — TUI preview length
- `sandbox_check` — per-tool path containment
- `output_cap` / `output_marker` — output truncation policy
- `completion` — whether this tool ends the turn (only Done)
- `title_detail` / `title_uses_secondaries` / `secondary_args` — TUI presentation
- `chat_only` — headless rejection gate
- `repair_args` — pre-validation argument repair (only Edit)
- `bypass_filter` — tool-filter bypass (only LoadSkill)

### Sandbox containment

Two layers, both in `sandbox.rs`:

- **`bash_check`** — static pre-flight analysis for shell commands. Splits
  compound commands on `;`/`&&`/`||`, tracks simulated `cd` state, strips
  heredoc bodies and subshell placeholder regions, and validates every
  path-looking token against canonical containment roots using
  `within_worktree_lexical` (leaf-symlink-tolerant). `cd` after `||` is not
  tracked (conservative). This is a heuristic — Bash subprocess I/O is not
  constrained by `io_enforce`, which only covers the file-tool handlers.

- **`io_enforce`** — application-level path containment function in
  `core/paths.rs`. Each file-tool handler (Read, Write, Edit, Glob, Grep,
  Landscape) calls it during `invoke()` to canonicalize the target path and
  check it against containment roots. `AllowAll` mode (no roots) causes each
  handler to skip the check. Not an OS sandbox, and not called from
  `Gremlin::init_runtime`.

### ToolSpec as single source of truth

`tool_specs.rs` contains the `TOOL_SPECS` constant — a table of every tool's
name, description, and parameter fields. Both `tool_definitions()` (model
schemas) and `validate_tool_args()` (dispatch validation) consume this table,
so a parameter added to one place is available to both. Some fields carry
`validated: false` (Read's `limit`/`offset`, Bash's `timeout`, Grep's
`path`/`glob`/`max_matches`, Glob's `path`) preserving byte-for-byte
compatibility with the old hand-written schema.

## Key invariants

- **Dispatch never names a tool for gate decisions.** Chat-only gate,
  completion detection, fan-out capping, display truncation, and filter
  bypass all use trait hooks, not name comparisons. When a tool is filtered
  out, the model sees "unknown tool", not a mode-revealing message.
- **Audit is best-effort.** The audit log is written under an `Arc<Mutex<()>>`
  and the write is skipped (not panicked) if the mutex is poisoned.
- **Sandbox roots are canonicalized once per check.** `bash_check` resolves
  roots once at the top and reuses them for every token.
- **Output capping is per-tool.** `Edit`, `Write`, `Done`, `Task`,
  `Mischief`, and `LoadSkill` are uncapped; `Read`, `Bash`, `Grep`, `Glob`,
  `Landscape`, and `WebSearch` are capped.
- **The global registry is a `OnceLock`.** It initializes once and is shared
  across all agent loops in the process.

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new tool | `handlers/` — add a new file + struct implementing `ToolHandler`, register in `handlers/mod.rs::default_registry`, and add its `ToolSpec` to `tool_specs.rs::TOOL_SPECS` |
| Change dispatch policy | `dispatch.rs::invoke_with_conversation` |
| Change tool filtering | `dispatch.rs` (tool-filter gate), `definitions.rs::tool_definitions`, `tool_specs.rs::always_available` |
| Change sandbox enforcement | `sandbox.rs` — `bash_check`, `check_path_tokens`, `check_tool` |
| Change argument validation | `schema.rs::validate_tool_args` and `tool_specs.rs` field tables |
| Change model-facing tool schemas | `definitions.rs::tool_parameters` and `tool_specs.rs::TOOL_SPECS` |
| Change per-tool presentation (TUI titles, labels) | The `title_detail`, `title_uses_secondaries`, `secondary_args` methods on each handler |
| Understand the runtime context available to handlers | `runtime.rs::AgentLoopRuntime` |
| Understand the ToolHandler contract | `registry.rs::ToolHandler` trait |
| Find tests | `tests.rs` (integration), plus per-handler test modules in `edit.rs`, `grep.rs`, `landscape.rs` |