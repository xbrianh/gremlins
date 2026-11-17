# Fixes applied — retire the dedicated debug path (review address)

## 1. `crates/gremlins/src/executor/supervisor/mod.rs` line ~244 — Correctness
**Issue**: Dropped `events_enabled_tx.send(true)` signal when old `"debug"` route was removed.
**Fix**: Added an 8-line comment explaining why the `"attach"` route intentionally does **not** unblock the global event-forwarding gate: attach clients hold the socket indefinitely, so unblocking would race broadcast events against the attach ack. The per-run bus subscription inside `handle_attach` handles events independently.

## 2. `crates/gremlins/src/executor/supervisor/attachment.rs` line ~291 — Correctness
**Issue**: Default `mode` flipped from `"passive"` to `"control"` without documenting the breaking change.
**Fix**: Added a 6-line `NOTE` comment documenting that the default changed as part of the debug→attach cutover, and that any `/watch`-style observer or third-party client that omits `mode` must now send `"mode": "passive"` explicitly to avoid rejection on non-agent leaves.

## 3. `crates/gremlins/src/executor/supervisor/debug/sequence_tests.rs` + `tests/debug_detach.rs` — Testing / Module boundaries
**Issue**: Two separate test files exercised the same `handle_conversation_attach` handler with stale `"talk"` op codes and fragile async-polling assumptions.
**Fix**: Consolidated both test files into `crates/gremlins/src/executor/supervisor/tests/attachment.rs`. Updated all test command payloads from `{"op":"talk",...}` to `{"op":"message",...}` to match production client behavior. Changed the detach `drive_handshake` test to wait for the `"waiting_for_ready"` status frame (which follows the `attached` ack and is emitted only after `target.attach()` succeeds and the pause is registered) instead of relying on single-poll continuation from the ack write.

## 4. `crates/gremlins/src/executor/supervisor/debug.rs` — Module boundaries
**Issue**: `debug.rs` housed `#[cfg(test)] mod sequence_tests` that tested `attachment::handle_conversation_attach`.
**Fix**: Removed the `sequence_tests` submodule declaration from `debug.rs`. The `debug` module now contains only its two helper functions (`send_debug_status`, `check_agent_stage`) and no test child.

## 5. `crates/gremlins/src/executor/supervisor/tests.rs` — Module boundaries
**Issue**: `mod debug_detach` declared a test module that lived outside the attachment module tree.
**Fix**: Replaced `mod debug_detach` with `mod attachment`.

## 6. `crates/gremlins-cli/src/main.rs` line ~1367 — Readability
**Issue**: The `"ready" | "attached"` match arm in the CLI debug reader task silently dropped display-variant frames without any logging, making it hard to distinguish display frames from the control frames consumed during handshake.
**Fix**: Added `log::debug!("debug: skipping display frame type={typ} (control variant already consumed)")` inside the arm.

## 7. `crates/gremlins/src/executor/supervisor/tests/paused_chat.rs` line ~123 — Testing
**Issue**: The `ended` frame assertion dropped the `ended["type"] == "ended"` check, leaving the frame-shape assertion weaker than before.
**Fix**: Added `assert_eq!(ended["type"], "ended")` before the `scope` and `reason` assertions, restoring the full shape check minus the now-present `scope` field.

## Files deleted
- `crates/gremlins/src/executor/supervisor/debug/sequence_tests.rs`
- `crates/gremlins/src/executor/supervisor/tests/debug_detach.rs`

## Files created
- `crates/gremlins/src/executor/supervisor/tests/attachment.rs` (merged from the two deleted files)