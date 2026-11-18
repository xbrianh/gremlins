# Address Summary

## Fix 1: `mod.rs` — `LogRelay` re-export compile error
**File**: `crates/gremlins/src/executor/supervisor/mod.rs`, line 60
**Category**: Correctness
**Issue**: `pub(crate) use self::run_registry::{LaunchResult, LogRelay}` re-exported `LogRelay` from `run_registry`, but `LogRelay` is only imported privately there (via `use super::lifecycle::LogRelay`). The compiler rejected this as "struct import `LogRelay` is private."
**Fix**: Split into two separate re-exports: `LaunchResult` from `self::run_registry` (where it's defined) and `LogRelay` from `self::lifecycle` (where it's defined). Both are `pub(crate)` in their home modules so the re-export is legal.

## Fix 2: `log_handler.rs` — `replay_history` doesn't signal client disconnect
**File**: `crates/gremlins/src/executor/supervisor/log_handler.rs`, lines 87–122, 279–285
**Category**: Correctness
**Issue**: `send_line` unsubscribes and returns `false` on write failure, but `replay_history` returned `(replayed, offset)` normally without signaling the disconnect. The orchestrator proceeded unconditionally into `drain_and_follow`, which tried to write to the dead client again, followed by a third `unsubscribe` at the end of `handle_log`.
**Fix**: Changed `replay_history` return type from `(HashSet<String>, u64)` to `Option<(HashSet<String>, u64)>`. On disconnect it returns `None`. The orchestrator now uses `let Some(...) = replay_history(...).await else { return; }` to bail immediately on client disconnect, skipping the follow phase and final unsubscribe.

## Fix 3: `lifecycle.rs` — unused `tokio::sync::watch` import
**File**: `crates/gremlins/src/executor/supervisor/lifecycle.rs`, line 5
**Category**: Cleanup
**Issue**: `use tokio::sync::watch;` was unused after `LaunchResult` (the only consumer featuring `watch::Receiver`) was moved to `run_registry.rs`.
**Fix**: Removed the import. The module already imports what it needs directly.

## Fix 4: `AGENTS.md` — stale file-map entries
**File**: `crates/gremlins/src/executor/supervisor/AGENTS.md`, lines 20–21, 106
**Category**: Readability
**Issue**: The `lifecycle.rs` row listed `spawn_gremlin_runner`, `launch_child`, and `stop_child` — all deleted. The `run_registry.rs` row omitted `LaunchResult`. The "Where to look" section pointed `launch_child`/`stop_child` at `lifecycle.rs`.
**Fix**: Updated `lifecycle.rs` row to list current API (`setup_gremlin_channels`, `run_gremlin_task`, `spawn_log_writer`, `reap_client_for`, `terminate_and_reap`, and `LogRelay`). Updated `run_registry.rs` to include `LaunchResult`. Updated "Where to look" to point at `run_registry.rs::RunRegistry::launch_child` / `stop_and_reap`.

## Fix 5: `chat/mod.rs` — duplicate `CancelToken` re-export
**File**: `crates/gremlins/src/executor/supervisor/chat/mod.rs`, line 46
**Category**: Readability
**Issue**: `CancelToken` was `pub(crate) use`-re-exported from both `chat/mod.rs` and `mod.rs`, creating two canonical import paths. `turn.rs` already imports `CancelToken` directly from `crate::clients::cancel::CancelToken` (line 36).
**Fix**: Changed the `pub(crate) use` in `chat/mod.rs` to a plain `use` (private import). The single public-facing re-export remains in `mod.rs` (test-only, via `super::CancelToken` in `tests.rs`).

## Not fixed (correct as-is)

- **`launch_run` cancel_token write-back** (`run_registry.rs` line 345): The write-back `gremlin.cancel_token = Some(Arc::clone(&cancel));` already exists in the code under review — the reviewer was reading a stale diff.
- **`LogOutput::log_sub` field type** (`log_handler.rs` line 64): Already declared as `&'a Option<Subscriber<EventKind>>` (single reference) — no extra `&`.