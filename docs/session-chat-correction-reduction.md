# Session-chat correction: reduction accounting

## Historical result: both reductions pass the conservative exclusion ledger

Baseline: **`9809155f8`**. Endpoint: **the tracked Rust working-tree snapshot
at the time of the audit**, not the stale `71e1354ea` snapshot. These historical
measurements include then-uncommitted Rust changes; they do not describe the
current working tree after the session-chat rename. The original audit changed
only this document and its Python script.

Define `H` as the concatenation `ho` + `me`. In prose code spans, `{H}` expands
to the exact historical path or identifier, not a current renamed file.

| Category | Additions | Deletions | Physical net |
|---|---:|---:|---:|
| Production Rust | 64 | 93 | **−29** |
| Test Rust | 555 | 620 | **−65** |
| All affected Rust | 619 | 713 | **−94** |

**Requirement 4 is conservatively certified for that audited snapshot:** production
is **−9** and tests **−4** after the exclusions below, independently.
Recomputed after `make fmt` passed. Physical lines include comments, blanks,
signatures, fixtures, assertions, and formatting; this is not a semantic
complexity or executable-statement metric.

## Every affected file

Paths are relative to `crates/gremlins-cli/src/tui/`.

| File | Production + | Production − | Net | Tests + | Tests − | Net |
|---|---:|---:|---:|---:|---:|---:|
| `app.rs` | 2 | 5 | −3 | 2 | 3 | −1 |
| `app/conversations.rs` | 21 | 11 | +10 | 81 | 114 | −33 |
| `client/{H}_tests.rs` | 0 | 0 | 0 | 1 | 12 | −11 |
| `entry_tests.rs` | 0 | 0 | 0 | 418 | 482 | −64 |
| `{H}_events.rs` | 16 | 27 | −11 | 0 | 0 | 0 |
| `mod.rs` | 25 | 50 | −25 | 51 | 7 | +44 |
| `screen.rs` | 0 | 0 | 0 | 2 | 2 | 0 |
| **Total** | **64** | **93** | **−29** | **555** | **620** | **−65** |

Inline tests total +136/−126, net **+10**. Test-only files total
+419/−494, net **−75**. Production growth in conversations is fully charged.

## Formatting exclusions: separate from physical totals

The following are conservative exclusions, not semantic-line measurements.
Savings are added back to the physical net; no test savings subsidize production.

### Production

Exclude the following 20 lines of savings, including comment shortening as an
extra precaution:

| Change | Excluded savings |
|---|---:|
| `app.rs`: shortened `observe_draft` call | 3 |
| `{H}_events.rs`: collapsed `apply_{H}_identity` signature | 5 |
| `mod.rs`: collapsed routing `match` | 3 |
| `mod.rs`: collapsed `clear_active_turn_ui` signature | 4 |
| `mod.rs`: shortened editor-gate comment | 2 |
| `mod.rs`: collapsed clear call (five old lines versus two new lines, including explicit interrupt reset) | 3 |
| **Total** | **20** |

The remaining production net is **−29 + 20 = −9** under this exclusion.
The substantive removals are the parallel `active_turn` value and its plumbing,
`{H}_busy`, the duplicate prompt rejection, and redundant terminal branches.
New authoritative-owner accessors and guards are charged as additions.
This is evidence of removed bookkeeping, not proof of semantic simplification.

### Tests: conservative negative result

Exclude **37** lines for the entire historical conversation-test saving, as
requested. The audited snapshot's conversation-test net is only −33; using 37 therefore
also charges four extra lines against the reduction. Do not describe −37 as the
audited snapshot's measurement.

Other identifiable shortened-call rewrapping savings include:

| Change | Excluded savings |
|---|---:|
| `app.rs`: correction assignment | 1 |
| `mod.rs`: correction assignment/`else` | 1 |
| `entry_tests.rs`: `{H}_event` signature (7 lines to 1, less 1 removed parameter line) | 5 |
| `entry_tests.rs`: `submit` signature (5 lines to 1, less 1 removed parameter line) | 3 |
| `entry_tests.rs`: stale-task `submit` chain (3 lines to 1) | 2 |
| `entry_tests.rs`: two collapsed identity calls (6 lines to 1 each, less 1 removed argument each) | 8 |
| `entry_tests.rs`: two collapsed native `{H}_bus` calls (5 lines to 2 each, less 1 removed argument each) | 4 |
| **Other exclusions subtotal** | **24** |

The historical exclusion ledger yields **−65 + 37 + 24 = −4**, versus the previous
**−53 + 37 + 24 = +8**. The extra 12-line reduction is genuine helper reuse,
not shortened-call rewrapping (details below). All 61 excluded test lines stay
excluded; expanded cases and helper bodies remain fully charged. Both categories
therefore pass independently under this conservative ledger. This is a scoped
manual accounting certification, not an exact semantic-code net or a general
proof about arbitrary formatting changes.

## Entry helpers and the claimed 106-line setup reduction

The audited `entry_tests.rs` snapshot physically shrinks by **64**, not 52 or 31. It introduces
`Fixture::attach`, `{H}_output`, `send_prompt`, `reply`, and `input_status`.
The eight-mode two-turn scenario includes enqueue rejection as well as visible,
parked, missing-done, missing-ended, correction, failure, and starting-correction.
The identity/interrupt test also gains cases. All expanded cases are charged.

A directly checkable consolidation is `Fixture::attach`: seven old 10-line
`attach_stage_entry` calls become seven one-line calls, at the cost of a new
13-line helper (including its separating blank). This saves **70 − 7 − 13 = 50**
physical setup lines. This is actual shared fixture plumbing, not rewrapping an
unchanged call. The transport helpers also centralize checked JSON-line writes.

Extra reuse: in `{H}_queued_draft_waits_for_any_agent_execution_then_delivers_exact_correction`
and `{H}_rejected_corrections_preserve_draft_and_require_explicit_retry`, two
7-line socket-release EOF assertions become two 1-line calls to the **existing**
`Fixture::assert_closed`. This removes **14 − 2 = 12** duplicated physical lines
with **zero new helper-body lines**. It preserves the two-second timeout,
read-success and EOF checks, and adds attachment target/task isolation and
no-send assertions through the existing helper. These savings are not excluded
as formatting and are already included in the physical totals, not subtracted
again.

**The stated 106 actual setup lines net before expanded cases was not verified.**
It must not be inferred from either the file net or the seven helper call sites.
A complete 106 ledger must specify which old transport/scenario setup blocks
are replaced, charge every new helper body, and exclude savings from merely
rewrapping calls after removing arguments. This audit does not certify that
number; the conservative pass above does not depend on that claim.

## Reproduction and classification

Run from the repository root. These commands perform a new audit of the selected
contents; the default working-tree run does not reproduce the historical totals
above after the rename. The then-uncommitted endpoint was not pinned by an
immutable revision here, so it requires the original snapshot to reproduce.
The optional immutable endpoint below reproduces only the older report.

```sh
# Defaults to baseline versus tracked working-tree contents (index + unstaged).
make fmt
python3 docs/session-chat-correction-reduction.py
git diff --numstat 9809155f8 -- '*.rs'
# Optional immutable endpoint, useful for reproducing the obsolete report:
python3 docs/session-chat-correction-reduction.py 71e1354ea
```

The optional tip is a positional Git revision. The baseline remains fixed.
Untracked files are outside Git's baseline diff and these totals. New working-tree
measurements are live snapshots, not immutable: rerun after any Rust changes.
The historical endpoint reproduces +54/−89 production (−35) and +382/−418 tests
(−36); those are not the audited snapshot's totals.

The script pins Myers diff, disables the indent heuristic, external diff,
text conversion, and renames, and reconciles each file against Git numstat.
It reports physical status and, only for the audited per-file counts, the
manual conservative exclusion result. Other snapshots require a fresh ledger
audit; historical endpoints receive physical status only. `make fmt`, the
reduction script (including numstat reconciliation), and `git diff --check`
passed for this refresh. No Rust source edits were made by this audit.

Deletions use old line numbers and additions use new line numbers. The four
mixed files have audited test-only suffixes beginning at their first top-level
`#[cfg(test)]` module:

| File | Baseline boundary | Audited snapshot boundary |
|---|---:|---:|
| `app.rs` | 1313 | 1310 |
| `app/conversations.rs` | 259 | 269 |
| `mod.rs` | 3242 | 3217 |
| `screen.rs` | 2215 | 2215 |

The suffix includes attributes, imports, helpers, tests and closing braces;
`mod.rs` includes both `attachment_tests` and `tests`. Earlier test module
declarations are unchanged. `entry_tests.rs` and `client/{H}_tests.rs` are
test-only files. `{H}_events.rs` is production throughout. This is an audited
range classifier, not a Rust parser. It fails on a changed file set or missing
boundary; other ranges require a fresh classification audit.
