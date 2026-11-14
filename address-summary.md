# Address Summary

## Fix 1: `crates/gremlins-magic-bus/src/connection.rs`, lines 6, 30
**Category**: Readability
**Issue**: `use tokio::io::AsyncWriteExt;` was declared mid-function (after `bytes.push(b'\n')`) rather than at the file top with other imports.
**Change**: Hoisted `use tokio::io::AsyncWriteExt;` to the top of `connection.rs`, alongside the other `use` declarations.

## Fix 2: `crates/gremlins/src/executor/mod.rs`, line 100
**Category**: Layering
**Issue**: `pub use gremlins_magic_bus::EventKind;` survived the Driver 3 cleanup, leaving a dead indirection in the executor's public API and a cross-layer coupling path where `clients::backends::cmd.rs` imported `EventKind` through the executor layer instead of directly from the bus crate.
**Change**: Removed the re-export line entirely.

## Fix 3: `crates/gremlins/src/clients/backends/cmd.rs`, line 21
**Category**: Cross-layer coupling
**Issue**: `use crate::executor::EventKind` made the clients layer transitively depend on the executor layer for a type that originates in `gremlins_magic_bus`.
**Change**: Replaced with `use gremlins_magic_bus::EventKind`.

## Fix 4–9: EventKind import redirects (6 files)
**Category**: Correctness (follow-on from Fix 2)
**Issue**: After removing the `pub use gremlins_magic_bus::EventKind` re-export, 6 modules that imported `EventKind` via `crate::executor::EventKind` would fail to compile.
**Change**: Redirected all `crate::executor::EventKind` imports to `gremlins_magic_bus::EventKind` in:
- `crates/gremlins/src/executor/stage_stream.rs:14`
- `crates/gremlins/src/executor/run.rs:29`
- `crates/gremlins/src/executor/run_agent.rs:22`
- `crates/gremlins/src/executor/run_exec.rs:13`
- `crates/gremlins/src/executor/exec_runner.rs:295,354`
- `crates/gremlins/src/executor/parallel.rs:1247,1254`

## Fix 10: `crates/gremlins/src/executor/supervisor/tests.rs`, lines 13, 300, 307, 1526
**Category**: Correctness
**Issue**: The `pub(crate) use gremlins_magic_bus::{BusSubGuard, ConnectionGuard, SharedWriter}` re-export was removed from `mod.rs`, but `tests.rs` referenced `super::SharedWriter` in its return type signature (line 300) and two call sites (lines 307, 1526) — these would fail to compile.
**Change**: Added `use gremlins_magic_bus::connection::SharedWriter;` to `tests.rs` imports and replaced all three `super::SharedWriter` references with `SharedWriter`.

## Fix 11: `crates/gremlins/src/executor/supervisor/AGENTS.md`, line 18
**Category**: Readability
**Issue**: The file map description stated `SharedWriter`, `BusSubGuard`, and `ConnectionGuard` "are re-exported here" — stale after Driver 3 removed the re-exports.
**Change**: Removed the "and are re-exported here" suffix from the table entry.

## Not fixed (non-issue)
- **`libc` dependency**: `libc = "0.2"` already exists at `crates/gremlins/Cargo.toml:11` — no change needed.
- **`write_json_line` behavioral drift**: The inline implementation in `connection.rs` (lines 28-34) is byte-for-byte identical to `socket::write_json_line` (lines 317-322) — same encode, same `\n` push, same `write_all`, same error strings. No drift risk.