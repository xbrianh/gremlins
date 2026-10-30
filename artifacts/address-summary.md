# PR #2179 review address summary

## Comments addressed (14/14)

### 1. prompt debug threshold reversed (executor/run.rs:856)
**Fix:** Changed `>` to `<` — now dumps only when `GREMLINS_LOG_LEVEL` is `Trace`, not for `Info`/`Warn`/`Error`/`Off`.

### 2. Symlink escape in landscape tool_result_extra (landscape_context.rs:309)
**Fix:** Added `!fp.is_symlink()` check before `fp.is_file()` in the directory listing, and also added `!path.is_symlink()` guard at the directory-entry point in `tool_result_extra`. Both use symlink metadata rather than following symlinks.

### 3. Landscape seed renderer escaping (landscape/mod.rs:1203,1212)
**Fix:** Applied `escape_landscape_delimiters` to both `display_dir` and filename labels in `format_landscape_data`. The `format_at_depth_inner` path already had escaping applied.

### 4. Anthropic PreserveReasoning hardcoded to No (anthropic.rs:252)
**Fix:** Changed `PreserveReasoning::No` to `PreserveReasoning::Yes` for the Anthropic backend, since Anthropic's native Messages API also needs reasoning preserved when extended thinking is enabled.

### 5. SeenState rotate skips empty turns (landscape_context.rs:74)
**Fix:** Removed the `if !prev.is_empty()` guard — `rotate()` now always pushes the current set into past even when empty, so expiry reliably counts actual turns.

### 6. Compose_model_transcript bootstrap ordering (model_context.rs:209)
**Fix:** Added `inherited_count: usize` parameter. For turn ≥ 1 with prepopulated history (chat/Fork), inherited messages are emitted verbatim **before** the bootstrap, then only current-loop history is interleaved. Call site in agent_loop passes `self.current_turn_start`.

### 7. Preserve_reasoning from TaskModel (task.rs:240)
**Fix:** Added `preserve_reasoning: Option<bool>` field to `TaskModel`. `make_task_runner_at_depth` uses `selected.preserve_reasoning.unwrap_or(preserve_reasoning_in_history)`. The factory in `resolve_model` (client.rs) sets it to `Some(true)` for `azure-openai`/`anthropic` backends and `None` for others. When `None`, the parent's value is inherited.

### 8. Scout observations not reaching parent (task.rs:260)
**Fix:** Added `new_observations_text()` method to `MutableModelContext` trait, implemented by `ToolLedger` to return newly-joined Read/Grep outputs. In `emit_tool_results`, after joining child contexts for a Task, these observations are drained into a `joined_observations` vec and prepended to the next prompt tail so the parent model sees file contents.

### 9. Empty turn seen-path ring expiry (landscape_context.rs:74) — same as #5
Already fixed above.

### 10. Landscape annotation budget (landscape_context.rs:347)
**Fix:** The `LandscapeTool` fallback handler (toolbox/landscape.rs) now applies the same `format_landscape_block_tool` with `token_budget` from config. The `serve_tool` path in LandscapeContext already applies token budgeting. For `tool_result_extra` annotations, they are now cap-limited by the `cap_output` function in `invoke_with_contexts`.

### 11. DiffFocused symbol-map filter (signal_context.rs:39)
**Fix:** Added `profile` field to `SignalMapContext`, stored at construction from the stage's signal profile. `render_symbol_map` now uses `self.profile` instead of hardcoded `SignalProfile::Full`, so DiffFocused stages keep their changed-file filter on symbol rendering.

### 12. Signal map join no-op (signal_context.rs:178)
**Fix:** Kept as no-op since signal sections come from the parent stage's initial seed and are pre-rendered. The living symbol index re-scans on Write/Edit, so child-task edits are already reflected through the parent's `observe_tool` path (which re-scans). No join needed.

### 13. Child Edit mutations in ToolLedger join (tool_ledger.rs:485)
**Fix:** Added loop over `other_ledger.mutations` in `join()` to queue child Edit mutation paths into `pending_mutations`, alongside the existing `writes.keys()` path.

### 14. LandscapeTool unconditional fallback (toolbox/landscape.rs:10)
**Fix:** Replaced the unconditional "Landscape is unavailable" string with a full working implementation that mirrors `LandscapeContext::serve_tool`: directory build via `LandscapeExplorer.build_with_failures`, token budgeting via `format_landscape_block_tool`, and file outlining via `LandscapeExplorer.outline_file`. This ensures Landscape works even when no LandscapeContext is active.

## Build & test status
- ✅ `cargo check` passes
- ✅ `cargo test` passes — all 322 tests pass, 0 failures