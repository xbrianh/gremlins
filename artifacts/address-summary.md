# Fixes Applied

## 1. `record()` – hash-mismatch branches were no-ops (lines ~2902-2909, ~2930-2937)
**Review**: Review 1 #1, #2; Review 2 #4
**Issue**: Both Read and Grep branches had identical `if`/`else` bodies — same-hash and different-hash both did `existing.count += 1; existing.last_age = age`. The content-hash check was decorative.
**Fix**: Different-hash path now resets `count = 1` and updates `seq` to the current sequence number, fully replacing the entry. Same-hash path only bumps `count`.

## 2. `record()` – `seq` used as age instead of `turn_num` (line ~2897)
**Review**: Review 2 #1
**Issue**: `last_age` was set to `self.seq` (a per-call monotonic counter), so `format(turn_num)` computed ages as `turn_num - call_counter` rather than `current_turn - stored_turn`.
**Fix**: `record()` now accepts `turn_num: usize`. `last_age` is set to `turn_num`. `seq` is separately tracked for ordering only.

## 3. `record()` – Edit tracked as Write (line ~2951)
**Review**: Review 2 #5
**Issue**: `"Write" | "Edit"` arm recorded Edit as `W(age)` and inserted a write entry. Plan says Edit is not tracked.
**Fix**: Split into separate arms. `"Write"` inserts `writes` entry + invalidates reads/greps/landscape. `"Edit"` only invalidates reads for the file + nukes greps/landscape, without emitting a write entry.

## 4. Call site – failed/denied results recorded (line ~2189)
**Review**: Review 2 #6
**Issue**: `record()` ran for every job including those that errored (`"Error: unknown tool"`, `"[exit N]"`, `"[timeout]"`).
**Fix**: Guard with `tools::result_status(&output) == "ok"` — only successful tool executions are recorded.

## 5. `format()` – headline age used `max` (oldest read) (line ~3036)
**Review**: Review 2 #2
**Issue**: `last_age = last_age.max(age)` selected the oldest (largest) age instead of the most recent (smallest).
**Fix**: Changed to `full_age = full_age.min(age)`, initialized to `usize::MAX`. Only full-file reads (offset=0, limit=0) contribute to the headline age; partial reads get their own suffix ages.

## 6. `format()` – `total_count` summed across partial reads (line ~3037)
**Review**: Review 2 #3
**Issue**: `total_count += entry.count` summed every partial-read entry into the single headline count.
**Fix**: Full-file reads contribute to `full_count`; partial reads each get their own count in suffix format (e.g., `.0-100:3t,2×` when count > 1). When only partials exist, they sum their counts into the headline but keep individual suffixes.

## 7. `format()` – partial-read suffixes in HashMap iteration order (line ~3038-3052)
**Review**: Review 2 #7
**Issue**: Partial reads were appended to the output in `HashMap` iteration order, producing non-deterministic output.
**Fix**: Entries are collected into a `Vec`, sorted by `entry.seq` before emitting suffixes.

## 8. `format()` – grep entries always after file lines (line ~3073-3084)
**Review**: Review 1 #3
**Issue**: File lines were emitted first, then grep entries, regardless of first-access seq. The plan required interleaving by first-access order.
**Fix**: Rewrote `format()` to collect `(first_seq, display_line)` tuples for files, greps, and landscape. All items are sorted by `first_seq` before emission, producing properly interleaved output.

## 9. `tool_ledger_dedup_by_content_hash` test (line ~3447-3450)
**Review**: Review 1 #6; Review 2 #10
**Issue**: Weak assertion `out2.contains(",2×") || !out2.contains("×")` trivially passed regardless of hash-mismatch behavior.
**Fix**: Tightened to `!out2.contains("×")` — when hash differs, the entry is replaced with count=1 (no count suffix).

## 10. `tool_ledger_format_ages` test (line ~3529)
**Review**: Review 2 #11
**Issue**: Test recorded at seq=0 and asserted `format(5)` → `R(5t)`, encoding the seq-minus-turn bug.
**Fix**: Now passes `turn_num=0` to `record()` and asserts `format(5)` → `R(5t)` (age = 5-0 = 5), and `format(1)` → `R(1t)` (age = 1-0 = 1). Ages are now true turn-relative values.