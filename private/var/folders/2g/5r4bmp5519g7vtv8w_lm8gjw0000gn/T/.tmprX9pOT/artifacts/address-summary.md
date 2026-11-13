# Fix Summary: supervisor.rs refactor review

## Fixes applied

### 1. `ops.rs` line 13 — unused `GremlinDefinition` import (Review 2)
- **Category**: Correctness
- **Resolution**: *No change needed.* The reviewer's finding was a false positive: `GremlinDefinition` is used as a trait for `.serialize()` on line 1055. Initially removed, compiler flagged it, restored. The import is correct; `GremlinDefinition` provides the `serialize()` method on `StaticDefinition`.

### 2. `util.rs` line 17 → `ops.rs` — bidirectional coupling (Review 3)
- **Category**: Coupling
- **Fix**: Moved `open_persisted_gremlin` from `ops.rs` into `util.rs`. `util.rs` no longer imports from `ops`. The coupling is now one-directional (ops → util only).

### 3. `mod.rs` `log_handler.rs` naming (Review 2)
- **Category**: Readability
- **Initially attempted**: Rename to `log.rs` per plan. Compiler rejected: `log` shadows the `log` crate, breaking all `log::info!()` / `log::warn!()` calls in `mod.rs`.
- **Fix**: Kept as `log_handler.rs`. The plan's naming is unachievable without renaming the crate dependency or adding `extern crate log as log_crate` everywhere.

### 4. `mod.rs` — re-export bloat and visibility widening (Review 2, Review 3)
- **Category**: Readability / Module boundaries
- **Fix**: Rewrote the re-export section to only export items actually consumed by `mod.rs` itself or external callers (`parallel.rs`, `run.rs`, `run_agent.rs`, `run_exec.rs`, `stage_stream.rs`, `backends/cmd.rs`). Items only referenced by tests moved into an `#[allow(unused_imports)]` block with a clear comment marking them as test-only re-exports. Removed the previous blanket re-exports of `teardown_session_impl`, `run_session_main_reader`, `handle_session_aux`, `verify_session_id`, `handle_session_op`, `StateView`, and many others that were unnecessarily widening visibility.
- **Result**: 14 re-exports removed from the crate-level surface. Two small `#[allow(unused_imports)]` blocks remain for `InteractiveChannels`/`InteractiveCommand` (tests use `super::InteractiveChannels`/`super::InteractiveCommand`) and the test-only re-exports.

### 5. `tests.rs` line 492 — JSON5 fixture indentation (Review 2)
- **Category**: Testing
- **Fix**: Restored original indentation of the `source:` block inside the JSON5 bootstrap fixture in `handle_launch_resolves_relative_filepath_against_project_root`. JSON5 is whitespace-insensitive so behavior was already correct, but the verbatim claim is now accurate.

### 6. `session.rs` line 21 — `log_handler` reference
- **Fix**: Updated the import from `super::log_handler::handle_log` (the name after initial rename to `log.rs` was reverted) to match the final `log_handler.rs` name.

## Verification
- `cargo check -p gremlins` — 0 errors, 0 warnings
- `cargo test -p gremlins --lib -- handle_launch_resolves_relative_filepath_against_project_root` — pass
- `cargo test -p gremlins --lib -- parallel_runtime_keys_classifies_markers_relative_to_target` — pass