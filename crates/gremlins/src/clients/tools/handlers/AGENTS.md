# clients/tools/handlers/ — concrete tool implementations

Each tool is a struct that implements the `ToolHandler` trait (defined in
`clients/tools/registry.rs`). The runtime layer in `clients/tools/` owns
gate/validate/audit/dispatch; these handlers own only the tool's actual
behavior. Every handler is a zero-sized struct (no state), registered in
`default_registry()`.

When adding a tool: create the handler file, implement `ToolHandler`
(at minimum `invoke`), register it in `mod.rs::default_registry`, and add
its `ToolSpec` to `tool_specs.rs::TOOL_SPECS`. The schema, validation, and
model-facing definition are all derived from the `ToolSpec` table — the
handler itself only needs to implement the trait methods that differ from
the defaults.

## File map

| File | Tool | Notes |
|---|---|---|
| `mod.rs` | — | Module declarations and `default_registry()` — the single place all 12 handlers are wired into a `ToolRegistry`. |
| `bash.rs` | Bash | Shell command execution. Spawns a process group via `command-group` so timeouts kill the whole tree. Streams stdout/stderr line-by-line with `LineScanner` for TUI live output. Applies environment variables via `apply_env_to_command`. |
| `done.rs` | Done | Completion signal. Returns its `summary` argument as prose. The only handler with `completion() == true` — the loop consults this to detect turn-end. |
| `edit.rs` | Edit | Batched file editing. `parse_edits` normalizes input formats, `apply_edits` applies chained search-and-replace with scope disambiguation, `render_success` produces context windows around changes, `edit_not_found_diagnostic` generates divergence reports for failed matches. The only handler with `repair_args()`. |
| `glob.rs` | Glob | File globbing. Wraps the `glob` crate with sandbox-aware path resolution. |
| `grep.rs` | Grep | Regex search. Walks directories, skips binaries, collects matches with configurable context lines, merges overlapping blocks, and separates non-contiguous results. |
| `landscape.rs` | Landscape | File/directory structural outline. Delegates to `gremlins-landscape` for the tree-formatted output. |
| `mischief.rs` | Mischief | Gremlins YAML DSL reference. Chat-only (`chat_only() == true`). Returns the full DSL reference document. |
| `read.rs` | Read | File reading with 1-based line number prefixes. Supports offset/limit pagination. Has a larger `display_truncation` (500) because its body is a focal artifact. |
| `skills.rs` | LoadSkill | Skill loading. Chat-only and bypasses the tool filter (`bypass_filter() == true`). Pushes resolved skill names into the session's `active_skills` list. |
| `task.rs` | Task | Fan-out worker delegation. Dispatches `Fork`/`Scout`/`Implement` tasks via the `task_fn` callback, which spawns nested agent loops. Implements `max_per_message()` to return `TASK_MAX_PER_TURN` (8). |
| `websearch.rs` | WebSearch | Web page fetching via curl. Chat-only. Formats truncated page text. |
| `write.rs` | Write | File writing. Creates or overwrites files, creating parent directories as needed. Sandbox-checked via `sandbox_check()`. |

## Trait method defaults vs per-tool overrides

Most trait methods keep their defaults. Here's what each handler overrides:

| Handler | Overrides |
|---|---|
| `BashTool` | `title_detail`, `secondary_args`, `sandbox_check`, `output_cap`, `invoke` |
| `DoneTool` | `completion`, `title_detail`, `invoke` |
| `EditTool` | `title_detail`, `sandbox_check`, `repair_args`, `invoke` |
| `GlobTool` | `title_detail`, `secondary_args`, `sandbox_check`, `output_cap`, `output_marker`, `invoke` |
| `GrepTool` | `title_detail`, `title_uses_secondaries`, `secondary_args`, `sandbox_check`, `output_cap`, `output_marker`, `invoke` |
| `LandscapeTool` | `title_detail`, `sandbox_check`, `output_cap`, `invoke` |
| `MischiefTool` | `chat_only`, `invoke` |
| `ReadTool` | `title_detail`, `title_uses_secondaries`, `secondary_args`, `display_truncation`, `sandbox_check`, `output_cap`, `invoke` |
| `LoadSkillTool` | `bypass_filter`, `invoke` |
| `TaskTool` | `title_detail`, `max_per_message`, `invoke` |
| `WebSearchTool` | `chat_only`, `title_detail`, `secondary_args`, `output_cap`, `invoke` |
| `WriteTool` | `title_detail`, `sandbox_check`, `invoke` |

## Key invariants

- **Handlers are stateless.** Every handler struct is a unit struct. All
  context (cwd, env, sandbox roots, telemetry channels) comes from
  `AgentLoopRuntime`, passed in at invoke time.
- **Handlers don't audit.** Audit logging happens in `dispatch.rs` after
  invoke returns. Handlers just return a `ToolOutcome`.
- **Sandbox policy lives in the handler.** Each handler's `sandbox_check()`
  receives the runtime context and parsed args, and returns `None` (allowed)
  or `Some(error)` (denied). The sandbox module (`sandbox.rs`) provides the
  building blocks (`bash_check`, `within_worktree`, etc.) but the per-tool
  policy of which args to check stays with the handler.
- **Bash is the most complex handler.** It manages process groups, live
  stdout/stderr streaming with `LineScanner`, environment variable injection,
  and timeout enforcement. Other handlers are straightforward single-call
  operations.
- **Edit is the second most complex.** Batched edits with scope
  disambiguation, chained application, divergence diagnostics, diff
  rendering, and legacy format repair. It's also the only handler with
  `repair_args()`.
- **Task delegates to the agent loop.** The handler itself only validates
  args and calls `task_fn`; the actual nested loop logic lives in
  `clients/agent/agent_loop.rs`.

## Where to look for…

| You want to … | Look at |
|---|---|
| Add a new tool handler | Any simple handler as a template (e.g. `done.rs` or `glob.rs`), then register in `mod.rs::default_registry` |
| Change Bash sandbox policy | `bash.rs::sandbox_check` and `sandbox.rs::bash_check` |
| Change how Edit matches/disambiguates | `edit.rs` — `find_all_matches`, `apply_edits`, `edit_not_found_diagnostic` |
| Understand live Bash output streaming | `bash.rs` — `LineScanner`, `read_both_streams`, `emit_live_lines` |
| Change how ProcessGroupGuard works | `bash.rs` — `ProcessGroupGuard`, `Drop` impl |
| Change Grep context behavior | `grep.rs` — `GrepCollector::append_file_matches`, `scan_file` |
| Understand Task fan-out | `task.rs::invoke` and `clients/agent/agent_loop.rs` |