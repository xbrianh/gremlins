# Plan11 code audit

## Findings and scope

This docs-only audit performed no Rust edits, builds, Rust tests, or manual runtime checks. That describes this audit's verification scope, not testing performed for the whole assignment by the Rust workers. Landscape was used before targeted Grep/read inspection; Git commands were read-only. The base-to-HEAD range includes accounting, home turns, exact input, and watch work, not an isolated plan11 commit. Do not attribute its entire growth to plan11.

**Important:** the plan11 removals inspected here are final **uncommitted worktree changes**, not landed HEAD changes. At the recorded HEAD there are no deleted Rust files against the base. The tables distinguish landed HEAD from the captured worktree. A fingerprint identifies the latter, but is not a recoverable source archive; reproducing historical worktree numbers requires the same bytes. The immutable HEAD numbers are independently reproducible.

## Measurement method

Run from repository root (Python 3, standard library only):

```sh
python3 -B docs/audit-plan11.py --base 45a8bd545 --target HEAD
python3 -B docs/audit-plan11.py --base 45a8bd545 --target worktree
# Immutable landed comparison: substitute the full recorded HEAD below for HEAD.
```

Scope is every Git-tracked `.rs` path, including unchanged crates; deleted paths count on the old side. Untracked Rust is excluded. Physical lines include blanks/comments and the final unterminated line. “Production” means the lexical non-test remainder, not compiled executable SLOC.

The lexer makes nested block comments, line comments, escaped normal/byte/C strings, raw strings with hash delimiters, and char literals opaque; lifetime apostrophes do not consume blocks. It finds exact `#[cfg(test)]` attributes and marks through the following balanced item/block or semicolon/comma, including attribute and boundary lines. Inline test modules therefore count even in production-named files. Whole integration-test directory files and external modules reached from test-only modules are test; ordinary module layouts and explicit `#[path = "..."]` are followed transitively. A physical line touched by a test span is entirely test. Text inside comments/strings cannot create cfg attributes or alter brace matching.

Limits: this is a lexical audit, not rustc/syn. It does not expand macros, evaluate compound cfg expressions/cfg_attr, resolve arbitrary include! or Cargo custom test targets, fully parse expression/field syntax, or handle arbitrary inline-module-relative #[path] layouts. Inner cfg(test) is treated as whole-file. Leading documentation/comments before a gated attribute stay production. A field/expression with a braced value can stop at its first balanced block. These conventions make the measurements precise and repeatable under this classifier, not exact compiled-code counts. No filename `_tests.rs` shortcut is used; explicit module declarations resolve such files.

Diffs use Python `difflib.SequenceMatcher(autojunk=False)` over physical source lines, independently per path, no rename detection. This is intentionally not Git's hunk algorithm; Git numstat can differ. Added lines use **new** labels, deleted lines use **old** labels. Unchanged lines can switch category when a gate changes; consequently production/test stock deltas need not equal category additions minus deletions. The transfer column below reconciles that difference. Total physical-line stock delta always equals additions minus deletions.

The script emits a SHA-256 manifest for all target Rust bytes and a fingerprint of compact sorted JSON `{path: sha256}`. Worktree reads are repeated and checked for stability; this detects ordinary concurrent edits but is not an atomic filesystem snapshot or protection against edit/revert races. HEAD is checked for movement. Non-Rust/index metadata are outside the Rust fingerprint.

## Remaining owners and duplication

Paths below are under `crates/`; function names are stable search anchors, rather than relying on line numbers.

| Area | Remaining owners after worktree removal | Assessment |
|---|---|---|
| Execution input | `gremlins/src/clients/execution_input.rs`: `InputRegistry`, `InputSender`, `InputReceiver`, receipts; `clients/interactive.rs`: `InteractiveSession::bind_input` and lifecycle; `executor/gremlin.rs::register_stage_inputs` | One queue/receipt owner with identity/lifetime bridges, not three competing inboxes. |
| Applying controls | `clients/agent/interactive.rs::inbox_boundary` (around 12–111) applies correction, pause, step, resume, bail; `InteractiveSession` stores pause/step state; `agent_loop.rs` and `turn_flow.rs` observe interruption at safe boundaries | State storage and scheduling remain split, but the observed command switch is centralized. Do not mistake cancellation/interruption tokens for a second debugger command queue. |
| Socket input | `executor/supervisor/input.rs::bind`, `command` (around 141–167), `handle_input`; CLI `gremlins-cli/src/input.rs::InputClient`, `validate_bind`, `validate_command`; TUI `tui/client.rs::selected_input_at` | Server authority, client receipt transport, TUI selection cache are adapters around the shared inbox. Remaining repeated protocol ownership: required-string extraction, target/command validation, JSON command vocabulary. Consolidation candidate is shared typed wire schema; retain validation on both trust boundaries. |
| Display | Bus `gremlins-magic-bus/src/event_kind.rs` and `bus.rs`; supervisor `attachment.rs`; CLI `watch.rs`, `tui/watch_replay.rs`, `tui/home_events.rs`, `tui/app/conversations.rs`, `tui/screen.rs` | Bus retention, socket framing/replay, home-event reduction, selected-run reduction, and rendering remain separate owners. Home/watch display reduction and history/live fallback are remaining overlap surfaces; removal is not a single unified transcript/rendering pipeline. Their scopes differ, so this inventory is not proof every repeated branch is redundant. |
| Other control surfaces | Supervisor chat/session turn lifecycle, TUI `send_control_request`/session requests, run stop/cancel | Session-turn cancellation and run lifecycle remain distinct from exact execution pause/step/bail. They should not be reported as eliminated by deleting debugger attachment. |
| Status/table display | CLI `main.rs::status_direct`, `direct_status_display*`, `print_table`; `display.rs` width/truncation; supervisor `ops/query.rs::handle_status`/`handle_info` | Direct disk fallback and daemon query paths still overlap status projection. Width utilities versus row rendering are not duplicate execution-control owners. |

The clearest concrete small duplication is CLI/server required-string and command validation. The larger remaining split is home versus selected-run display/state reduction. No claim is made that every adapter above should be deleted. At landed HEAD, legacy interactive/debug attachment code still coexists with the exact-input path; only the worktree removes that ownership, so claiming landed plan11 completion would be incorrect.

## Final worktree measurements

The following data are generated from the accompanying classifier. `P transfer` is production stock delta minus `(P additions - P deletions)`; the test transfer is its negation. All numbers are physical lines.

- Captured UTC: `2026-11-26T18:29:26.970257+00:00`.
- Base: `45a8bd5450f27a80e85a91f5110206d6bbd9fce4`.
- HEAD: `f00af55fb2e31a91ff5bf16a8ce55ebfe4682493`.
- Worktree Rust fingerprint: `9f4514c8e7f02c1b521e863b1e9d428f0bec10c53e3e84316bb1ed2db7ad8857`.
- Script SHA-256: `05bc83f8e2f1b3663a196a1d1ad6e05a614f110452338127122a0709eb65bc4d`.
- Final capture after Rust workers finished; the script's repeated worktree read and an independent subsequent complete Rust capture matched the recorded fingerprint. This records the final current worktree, not a landed commit.
- Rust files: base **233**, HEAD **255**, final worktree **251**.

### Stock (production / test)

| Scope | Base P | Base T | HEAD P | HEAD T | Worktree P | Worktree T |
|---|---:|---:|---:|---:|---:|---:|
| **Workspace** | **75,137** | **66,358** | **79,947** | **88,810** | **76,851** | **81,943** |
| drag | 5,343 | 2,889 | 5,343 | 2,889 | 5,343 | 2,889 |
| gremlins | 49,483 | 49,557 | 52,454 | 64,698 | 49,576 | 57,860 |
| gremlins-cli | 16,920 | 11,655 | 18,534 | 18,541 | 18,319 | 18,486 |
| gremlins-landscape | 1,622 | 1,238 | 1,622 | 1,238 | 1,622 | 1,238 |
| gremlins-magic-bus | 1,769 | 1,019 | 1,994 | 1,444 | 1,991 | 1,470 |

Landed stock growth: **+4,810 production, +22,452 test**. Final worktree growth against base: **+1,714 production, +15,585 test**. Worktree versus HEAD stock: **−3,096 production, −6,867 test**. Thus the removal reduces code substantially relative to HEAD but does not make the entire base-to-worktree range net-negative.

The net production increase versus `45a8bd545` includes added responsibilities: exact execution input with receipt lifecycle and identity/lifetime handling, watch replay and history/live reconciliation, and home conversation/turn orchestration. These are not merely replacement debugger plumbing. Their addition explains why retiring legacy control ownership can coexist with net production growth across this broader range; this audit does not assign an exact line budget to each responsibility.

### Additions / deletions against base, classified on each side

| Target / scope | P + | P − | T + | T − | P transfer |
|---|---:|---:|---:|---:|---:|
| **HEAD workspace** | **7,429** | **2,534** | **23,839** | **1,472** | **−85** |
| HEAD gremlins | 4,481 | 1,432 | 15,565 | 502 | −78 |
| HEAD gremlins-cli | 2,692 | 1,071 | 7,819 | 940 | −7 |
| HEAD magic-bus | 256 | 31 | 455 | 30 | 0 |
| **Worktree workspace** | **7,011** | **5,213** | **22,451** | **6,950** | **−84** |
| Worktree gremlins | 4,071 | 3,901 | 14,190 | 5,964 | −77 |
| Worktree gremlins-cli | 2,683 | 1,277 | 7,780 | 956 | −7 |
| Worktree magic-bus | 257 | 35 | 481 | 30 | 0 |

`drag` and `gremlins-landscape` have zero additions/deletions/transfers in both comparisons. Worktree total physical delta: `(7,011 + 22,451) − (5,213 + 6,950) = 17,299`; this equals `1,714 + 15,585`.

### Selected files: base → captured worktree

Paths are relative to `crates/`. The script's `changed_files` output supplies the complete per-file inventory.

| File | Base P/T | Worktree P/T | P +/− | T +/− |
|---|---:|---:|---:|---:|
| gremlins/src/clients/interactive.rs | 1,139 / 800 | 373 / 33 | 238 / 994 | 16 / 793 |
| gremlins/src/clients/execution_input.rs | 0 / 0 | 399 / 517 | 399 / 0 | 517 / 0 |
| gremlins/src/executor/supervisor/attachment.rs | 1,342 / 759 | 573 / 1,138 | 380 / 1,149 | 1,070 / 691 |
| gremlins/src/executor/supervisor/input.rs | 0 / 0 | 276 / 594 | 276 / 0 | 594 / 0 |
| gremlins-cli/src/input.rs | 0 / 0 | 270 / 324 | 270 / 0 | 324 / 0 |
| gremlins-cli/src/tui/client.rs | 773 / 853 | 851 / 1,372 | 185 / 107 | 717 / 198 |

### Deleted legacy files

**None are deleted in landed HEAD against base.** These four tracked paths are absent only in the final worktree:

- `crates/gremlins/src/executor/active_leaf.rs`: 116 base production and 95 base test lines; legacy active-leaf tracking.
- `crates/gremlins/src/executor/supervisor/debug.rs`: 23 base production lines; legacy debugger dispatch file.
- `crates/gremlins/src/executor/supervisor/tests/attachment.rs`: 2,458 base test lines; legacy attachment suite. Deletion does not establish equivalent replacement coverage.
- `crates/gremlins-cli/src/tui/client/attachment.rs`: 144 base production lines; legacy TUI attachment helper.

Most production retirement is **within retained files**, especially `clients/interactive.rs` and supervisor `attachment.rs`, rather than whole-file deletion. The retained module names alone do not demonstrate old ownership survives, nor do the deleted files alone demonstrate all duplication is gone.
