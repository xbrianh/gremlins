# Phase 1 review fixes

All three reviews addressed. 9 fixes applied, 2 concerns investigated and confirmed non-issues.

## Fixes applied

### 1. `config.rs` — `GREMLINS_LANDSCAPE_SIGNALS=1` didn't parse (Correctness, Review 2 #1)
`"1".parse::<bool>()` only accepts `"true"`/`"false"`. Added `parse_env_bool()` helper accepting `"1"`/`"0"`/`"true"`/`"false"` (case-insensitive). Applied in both the env-only fallback branch (line ~345) and the settings-file branch (line ~545).

### 2. `signals.rs:build_changed_files` — git paths resolved against wrong root (Correctness, Review 2 #5)
`git diff --name-only` emits paths relative to the repository root, but the code joined them to `ctx.cwd` — wrong when cwd is a subdirectory. Now runs `git rev-parse --show-toplevel` first and joins paths to the repo root. Also added `canonical.is_file()` guard to skip paths deleted since HEAD~10.

### 3. `signals.rs:build_changed_files` — missing doc for `HEAD~10` (Readability, Review 2 #6)
Added doc comment explaining the fixed-depth snapshot rationale (deep enough for context, bounded for conciseness) and noting that uncommitted changes are excluded because the landscape block is a stage-start snapshot.

### 4. `signals.rs` — removed `#[allow(dead_code)]` from 5 live functions (Readability, Review 1, Review 2 #3)
Removed from `contained_path`, `build_changed_files`, `build_referenced_paths`, `build_symbol_map`, `render_signal_sections`. All are transitively live via `render_signal_sections` → `inject_signal_sections` → `landscape_block`. Also removed from the `identifiers` field on `SignalRendered`.

### 5. `signals.rs:render_signal_sections` — removed dead `_prompt` parameter (Readability, Review 1, Review 2 #7)
Removed `_prompt` from `render_signal_sections` and unwound the call chain: `landscape_block` no longer accepts a `prompt` parameter, and the `run_agent` prompt-resolution reordering was reverted.

### 6. `run.rs:build_landscape_snapshot` — `#[allow(dead_code)]` on dead production code (Module boundaries, Review 2 #3, Review 3 #2)
Moved to `#[cfg(test)]`. No `#[allow(dead_code)]` annotation. Extracted shared `format_outlines` helper that both `landscape_block` and `build_landscape_snapshot` call.

### 7. `signals.rs:referenced_paths_rejects_sandbox_escape` — vacuous test assertion (Testing, Review 1, Review 2 #9)
Changed `if let Some(ref text) = referenced { assert!(!text.contains(...)) }` to explicit `assert!(referenced.is_none(), ...)`. The previous form skipped the assertion entirely when all paths were correctly rejected.

### 8. `run.rs:landscape_block_signals_off_is_identical` — test name misleading (Testing, Review 1)
Renamed to `build_landscape_snapshot_matches_format_landscape_block` — it validates internal refactoring consistency, not `landscape_block` behavior.

### 9. `mod.rs` — layering: `run.rs` reached directly into `landscape::signals` (Layering, Review 3 #1)
Added `pub(crate) fn inject_signal_sections(...)` wrapper in `landscape/mod.rs`. `run.rs` now calls that instead of `crate::landscape::signals::render_signal_sections` directly.

### 10. `run.rs:landscape_block` — section order reversed (Readability, Review 2 #10)
Prepending sections in loop reversed the display order. Now iterates with `.rev()` so the final block reads Changed files → Symbol map → Referenced files → skeleton.

## Non-issues (investigated, no change needed)

### `signals.rs:contained_path` canonical-vs-non-canonical root (Review 2 #4)
`within_worktree` internally calls `normalize_path` on both the candidate path and each root, which canonicalizes both. The non-canonical `root` passed from `contained_path` is canonicalized internally before comparison — no inconsistency.

### `signals.rs` coupling to `clients::tools` (Review 3 #4)
The plan explicitly authorizes importing `canonicalize_or_ancestor` and `within_worktree` from `crate::clients::tools` ("No new dependencies. Sandbox primitives...are re-exported or accessible from `clients/tools.rs`"). Extracting these to a shared utility module is deferred to a separate refactoring pass.