# Fixes Applied

## 1. `watch_replay.rs`, line 68 (`live_fingerprint`) — Correctness

**Issue:** Structural-event exclusion matched on classified kind strings (e.g.
`"tool_output_chunk"`), but `classify_stream_value` maps `"tool_output_chunk"` →
`"tool_output"`. The match arms never fired, so structural events became
dedupable — a live event whose text matched a replayed fingerprint would be
silently dropped.

**Fix:** Match raw `raw.get("type")` tags for the structural-exclusion check
before delegating to `classify_stream_value`, matching the pre-refactor
behavior exactly. Added a comment explaining the vocabulary-mismatch risk.

## 2. `watch_replay.rs`, line 297 (`replay_and_dedup`) — Performance

**Issue:** The inner `break` on `log_tx.send` failure only exited the
per-envelope loop; the outer `for line in lines` loop continued decoding up to
500 blob lines after the receiver was gone.

**Fix:** Added a `'replay:` label on the outer loop and changed the inner
`break` to `break 'replay;` so the entire replay path aborts on first send
failure.

## No changes needed (verified)

- **`find_stage_type_in_tree` in `screen.rs`**: Already deleted — grep
  confirms zero occurrences in the codebase.
- **`AbortOnDrop` field visibility (`pub(crate)`)**: Required for tuple-struct
  construction (`AbortOnDrop(handle)`) from `watch_replay.rs`. Reviewer #2's
  claim that "nothing accesses `.0`" misses that tuple-struct construction
  IS field access in Rust.
- **Unknown-type fallback in `classify_stream_value`**: The `_ =>` arm falls
  through to `value.get("kind")`, which returns `None` for arbitrary JSON
  without a `"kind"` field. Since `classify_stream_value` returns
  `Option<String>`, the `?` in `live_fingerprint` propagates `None`, matching
  the old `_ => return None` behavior.