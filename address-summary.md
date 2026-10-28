# Review fixes — Phase 4 Slice 2

## Fix 1: DiffFocused empty-diff falls back to Full sections
**File:** `crates/gremlins/src/landscape/signals.rs`, lines 498–550
**Category:** Correctness (both reviewers)
**Change:** Restructured `render_signal_sections` so `build_changed_files` is called once before the profile dispatch. When `DiffFocused` yields empty `diff_paths` (non-git cwd, empty diff), the code falls through to emit the full (unscoped) symbol map and referenced-files sections instead of early-returning with zero sections. This preserves the plan constraint "non-git content roots get Full profile, no regression."

## Fix 2: Signal injection gate uses `!signal_sections.is_empty()` instead of `file_count > 0`
**File:** `crates/gremlins/src/executor/run.rs`, line 131
**Category:** Correctness (both reviewers)
**Change:** The guard `signals_enabled && file_count > 0` used `file_count` from the *diff-filtered* skeleton. A diff touching only files that produce no outlines (docs, config, deleted files) would drop the changed-files section entirely. Changed to `!signal_sections.is_empty()` so signal sections are injected based on whether sections were produced, not whether the skeleton has outlineable files.

## Fix 3: Loose substring matching on artifact keys
**File:** `crates/gremlins/src/executor/run.rs`, line 787
**Category:** Correctness (review 2)
**Change:** `k.contains("artifact://diff")` matched unrelated keys like `artifact://diffmap` or `artifact://diff-lint`. Changed to `k == "artifact://diff" || k.starts_with("artifact://diff/")` for exact/delimited-prefix matching.

## Fix 4: Redundant log condition and unconditional "profile Full" noise
**File:** `crates/gremlins/src/executor/run.rs`, lines 98–101
**Category:** Readability (both reviewers)
**Change:** Inside the `if signals_enabled` block, `else if signals_enabled` was tautological. Changed to plain `else` and gated the "profile Full" log on `crate::config::landscape_stage_profiles()` so it only fires when the profiles feature is actually enabled, eliminating log noise in the default-off path.

## Fix 5: Missing negative assertion in profile scoping test
**File:** `crates/gremlins/src/landscape/signals.rs`, line 1277
**Category:** Testing (review 1)
**Change:** Added `assert!(!body.contains("func_00"), "symbol map should NOT contain func_00 (outside diff)")` next to the existing `func_01` presence check, verifying that out-of-diff symbols are actually excluded by the scoping filter.
