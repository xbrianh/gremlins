# Fix Summary — Diff Stat Summary (Phase 4 Slice 3)

## Fixes applied

### 1. `crates/gremlins/src/landscape/signals/mod.rs` — Test helper `git_commit_all` (before my turn)
- **Issue**: Needed a helper to commit all tracked/untracked changes in one shot for test setup.
- **Fix**: Added `fn git_commit_all(dir, msg)` wrapping `git add -A && git commit -q`.

### 2. `crates/gremlins/src/landscape/signals/mod.rs` — `is_file()` gate dropped (before my turn)
- **Category**: Correctness — Review 2, finding 1.
- **Issue**: `canonical.is_file()` silently dropped fully-deleted files from the stat, defeating the section's purpose of surfacing deletion magnitude.
- **Fix**: Removed the `is_file()` check; the changed-set membership and containment/pruning already encode the existence policy.

### 3. `crates/gremlins/src/landscape/signals/mod.rs` — `--no-renames` + quotePath handling (before my turn)
- **Category**: Correctness — Review 2, finding 3.
- **Issue**: `git diff --numstat` rename entries (`old => new`) and `core.quotePath`-quoted paths produced wrong display strings.
- **Fix**: Passed `--no-renames` to git diff; added quote-path unquoting logic.

### 4. `crates/gremlins/src/landscape/signals/mod.rs` — `SignalSection` key/name split (before my turn)
- **Category**: Readability — Review 2, finding 4.
- **Issue**: Section name `"Diff stat"` was used as both display name and log key, coupling the executor to the exact title string.
- **Fix**: `SignalSection` now carries a stable `key` field (e.g. `"diff-stat"`) alongside the display `name`; the executor logs `section.key` directly.

### 5. `crates/gremlins/src/landscape/signals/mod.rs` + `crates/gremlins/src/executor/run.rs` — Config gate moved to `SignalContext` (before my turn)
- **Category**: Layering — Structural review.
- **Issue**: `landscape_diff_stat()` was checked inside `signals/mod.rs`, while sibling gate `landscape_stage_profiles()` was checked in `executor/run.rs` and passed via `SignalContext`.
- **Fix**: Added `diff_stat: bool` to `SignalContext`; executor sets it from config; `render_signal_sections` uses `ctx.diff_stat`.

### 6. `crates/gremlins/src/landscape/signals/mod.rs` — Perl regex fix for `SignalContext { … }` in tests
- **Category**: Build correctness.
- **Issue**: Prior perl substitution (`prompt: Some([^)]+),`) failed on `prompt: Some("…".to_string()),` because the nested `)` from `to_string()` tripped the negated character class. Six test `SignalContext` literals were missing `diff_stat: false`.
- **Fix**: Rewrote regex to use non-greedy `.+?` and re-ran substitution; all 20+ `SignalContext` literals now carry `diff_stat`.

### 7. `crates/gremlins/src/landscape/signals/mod.rs` — Unused `diff_paths` variables
- **Category**: Warning cleanup.
- **Issue**: Two tests (`diff_stat_drops_binary_and_pruned_and_escaped`, `diff_stat_caps_at_30`) captured `diff_paths` from `build_changed_files` but no longer passed it to `build_diff_stat`.
- **Fix**: Renamed to `_diff_paths` in both tests.

### 8. `crates/gremlins/src/landscape/signals/mod.rs` — Broken `diff_stat_empty_when_no_git_or_no_changes` test
- **Category**: Test correctness.
- **Issue**: The second sub-case asserted `build_diff_stat(&ctx2).is_none()` for a git repo, but `build_diff_stat` no longer accepts a changed-set filter — the gating moved to `render_signal_sections`. The assertion was now false (a real git repo produces stats).
- **Fix**: Renamed test to `diff_stat_empty_when_no_git`, removed the invalid sub-case. The "empty changed set yields None" semantic is now covered by the new dispatch-level test.

### 9. `crates/gremlins/src/landscape/signals/mod.rs` — Dispatch-level tests (Review 2, finding 5)
- **Category**: Testing.
- **Issue**: No test exercised `render_signal_sections` with the diff-stat gate: section ordering, flag-on/flag-off behavior, and prepend-before-changed-files were untested.
- **Fix**: Added two tests:
  - `diff_stat_section_prepended_when_enabled`: verifies `"diff-stat"` is the first section and precedes `"changed-files"` when `diff_stat: true`.
  - `diff_stat_section_absent_when_disabled`: verifies `"diff-stat"` is absent when `diff_stat: false` and present when `diff_stat: true`.