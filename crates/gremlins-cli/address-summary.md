# Fix Summary: Eliminate ChatEvent, route on EventKind

## event_kind.rs (gremlins-magic-bus)

1. **Line 78-80 — Correctness**: Removed the `loop_turn: Option<usize>` field from `EventKind::TurnComplete`. The `turn: usize` field already carries the loop counter, and adding `loop_turn` duplicated it in the wire format (emitting both `"turn"` and `"loop_turn"` with the same value), violating the "wire format stays identical" constraint.

2. **Line 184 — Correctness**: Removed `Ready` from `is_chat_event()`. `Ready` was previously handled via `event_tx`. Including it in `is_chat_event()` routed it to `chat_tx`, where the `_ => {}` catch-all silently discarded it, breaking the documented path.

3. **Line 200 — Correctness**: Removed `TaskProgress` from `is_stream_event()`. The original hand-maintained blacklist did not include `TaskProgress`, so adding it changed control-socket forwarding behavior beyond the plan's scope. The old blacklist is now preserved exactly.

## client.rs (gremlins-cli)

4. **Line 466-468 — Readability**: Updated `connect()` doc to enumerate all three receivers in the return type: lifecycle events, chat events with turn tags, and raw JSON lines.

5. **Lines 572-608 — Correctness**: Restored the id-presence discriminator for `EventKind::Error` and `EventKind::SystemWarning`. Only routes these to `chat_tx` when the `chat_turn` tag is present on the JSON line. Bare `type:"error"` response lines now correctly fall through to the FIFO response queue, matching the old `parse_chat_event` behavior (which only classified id-bearing errors as chat).

## chat/mod.rs (gremlins supervisor)

6. **Lines 394-405 — Correctness + Coupling**: Rewrote the unresolved-skills warning to serialize through `chat_payload()` (which tags it with `chat_turn` for stale-event filtering) and removed the `emit_event` bus call. The warning was previously delivered on two paths (direct socket write + global bus), risking duplicated transcript lines and leaking session-scoped warnings to other sessions.

7. **Lines 617-628 — Correctness + Coupling**: Removed the `emit_event(EventKind::Error { ... })` call on `Recv::Closed`. The error is already delivered to the session socket via `stream_terminal`; broadcasting it on the global bus duplicates it and can leak to other sessions.

8. **Lines 640-649 — Correctness**: `TurnComplete` forwarding no longer reconstructs the event with `loop_turn: Some(turn)`. It only updates `totals` (baseline-adjusted) and forwards the rest unchanged. The loop counter stays in the existing `turn` field.

9. **Lines 493-510 — Correctness**: `chat_payload()` now wraps non-object values in `{"value": ...}` before tagging, preserving the old behavior. Previously the arm `_ => return fields` silently dropped the `chat_turn` tag for non-object payloads.

## mod.rs (gremlins-cli TUI)

10. **Lines 2527 — Readability**: Replaced the 12-line no-op `event_turn` match (every arm yielded `turn.clone()`) with a single `let event_turn = turn.clone();`.

11. **Lines 2571-2593 — Correctness**: `TurnComplete` now destructures `turn` directly instead of `loop_turn`. Uses `turn` for the stats title.

12. **Line 3 + chat.rs file — Module boundaries**: Removed `pub mod chat;` declaration and deleted the empty `crates/gremlins-cli/src/tui/chat.rs` file. Also updated `AGENTS.md` to remove the dead module reference.

## turn_flow.rs (gremlins agent)

13. **Line 459 — Correctness**: Removed `loop_turn: None` from the `EventKind::TurnComplete` construction, since the field no longer exists.

## AGENTS.md (gremlins-cli)

14. **Line 15 — Documentation**: Removed the now-stale `chat.rs` row from the file table.