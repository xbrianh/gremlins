# Fixes Applied

## 1. `crates/gremlins/src/executor/supervisor.rs` — lines 2035–2043
**Category**: Correctness (review 2, finding 1)
**Issue**: The running-guard checked only `"running"`, ignoring other non-terminal states, and the check-then-act window allowed a race between the status read and the blob write.
**Fix**: Changed the guard to reject any status that is not `"done"` or `"stopped"`, with a clear error message that quotes the actual status. Moved the status read to a local variable just before `open_blob`, narrowing the race window.

## 2. `crates/gremlins/src/executor/supervisor.rs` — lines 2096–2100
**Category**: Correctness (review 1, finding 1)
**Issue**: `blob.write_all(&bytes)` was not followed by a flush or explicit drop. Since `StateBlob` abstracts over a raw `File`, unwritten kernel-buffer bytes could be lost before `ok_response` returns.
**Fix**: Added `blob.flush().map_err(…)?` after `write_all`, then an explicit `drop(blob)` before constructing `ok_response`.

## 3. `crates/gremlins/src/executor/supervisor.rs` — line 26
**Category**: Readability (review 2, finding 4)
**Issue**: `GremlinDefinition` was flagged as unused. Verification showed it *is* required — it provides the `serialize()` trait method called on `StaticDefinition`. Reverted the removal.
**Fix**: Restored `GremlinDefinition` in the `use` import (no-op; confirmed necessary by `cargo check`).

## 4. `crates/gremlins-cli/src/main.rs` — lines 1013–1020
**Category**: Correctness (review 2, finding 5)
**Issue**: `--list` silently ignored conflicting `--put` or positional `<artifact>` args.
**Fix**: Added explicit mutual-exclusion checks: `--list` with `--put` returns an error; `--list` with an `<artifact>` URI returns an error.

## 5. `crates/gremlins-cli/src/main.rs` — lines 1038–1047
**Category**: Readability (review 2, finding 6) + Correctness (review 2, finding 5)
**Issue**: The usage error listed `--list` and `--put` as alternatives when an `<artifact>` URI was missing, but both `--put` and `--json-path` still require the URI. Also `--put` combined with `--json-path` silently dropped `json_path`.
**Fix**: Reworded the error to `"<artifact> URI is required (use --list to enumerate registered keys)"`. Added an explicit mutual-exclusion check between `--put` and `--json-path`.

## 6. `crates/gremlins-cli/src/main.rs` — lines 1065–1081
**Category**: Correctness (review 2, finding 7) + Readability (review 1, finding 2)
**Issue**: Missing/invalid `keys`, `content`, and `path` fields from daemon responses were silently defaulted to empty, hiding protocol drift. Artifact content was printed via `print!` (no trailing newline), causing prompt confusion.
**Fix**: Changed each `.unwrap_or("")` / `.unwrap_or_default()` to `.ok_or_else(|| "daemon response missing …")?` so protocol mismatches produce hard errors and non-zero exit. Changed `print!("{content}")` to `println!("{content}")` for consistent output with the write path.