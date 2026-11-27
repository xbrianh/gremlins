# Watch selection: pre-change to final checkout

## Baseline and scope

Baseline: `784d4fca4d22ebc3e3538732fac91bdc0c849955` (`HEAD` at measurement).
Target: the working tree, including untracked source files and excluding missing
files. This is a working-tree comparison, not just the tracked `git diff`.

The production scope is **all Rust paths under
`crates/gremlins-cli/src/tui/`**, with test-only physical lines removed on each
side before diffing. The affected subtotal includes only paths whose original
bytes differ. Other CLI code, executor changes, prompts, and documentation are
not TUI production. Blank lines and comments outside test-only items count.

Test-only means entire external test modules and every `#[cfg(test)]` item,
including inline functions, fields, statements, and test hooks—not just
`mod tests`. The Python lexer/brace matcher in `docs/audit-plan11.py` identifies
these spans while ignoring braces in strings and comments; its external-module
closure identifies test files, including test-support files without a test
filename. A line touched by a test-only span is assigned wholly to tests.
Production and test line streams are diffed separately per **raw path** using
Git's default text diff. There is no rename matching: moving a file counts its
full deletion and full addition, even if its contents are identical.

## Results

| Scope | Before | After | Additions | Deletions | Net |
|---|---:|---:|---:|---:|---:|
| Affected TUI production paths | 12,350 | 12,407 | 1,497 | 1,440 | +57 |
| All TUI production paths | 15,084 | 15,141 | 1,497 | 1,440 | +57 |
| TUI tests (external files and inline test-only lines) | — | — | 4,631 | 3,289 | +1,342 |
| Documentation/prompt/audit scripts and measurement text, defined below | — | — | 660 | 579 | +81 |

The previously supplied **15,071 → 15,103** endpoints are **not reproduced** by
this physical-line scope: they were 13 lines below the pre-acknowledgement
measurement endpoints, not the final endpoints shown above.
Those endpoints and the earlier **+32 net** predate the acknowledgement fix;
neither describes the final checkout. The final net is **+57**. No arbitrary
13-line adjustment is applied.

### Production ledger

Current paths below are relative to `crates/gremlins-cli/src/tui/`.
The two former-module rows use responsibility descriptions, not verbatim
historical path labels. Their measured values are unchanged; both sides of
these moves remain counted independently.

| Path | Before | After | Added | Deleted | Net |
|---|---:|---:|---:|---:|---:|
| `app.rs` | 1,286 | 1,311 | 53 | 28 | +25 |
| `app/conversations.rs` | 165 | 206 | 68 | 27 | +41 |
| `client.rs` | 878 | 879 | 30 | 29 | +1 |
| Former session-chat client module (descriptive) | 733 | 0 | 0 | 733 | −733 |
| `client/session_chat.rs` | 0 | 733 | 733 | 0 | +733 |
| `commands.rs` | 371 | 336 | 6 | 41 | −35 |
| Former session-chat event module (descriptive) | 260 | 0 | 0 | 260 | −260 |
| `session_chat_events.rs` | 0 | 269 | 269 | 0 | +269 |
| `mod.rs` | 3,203 | 3,146 | 225 | 282 | −57 |
| `protocol.rs` | 56 | 56 | 0 | 0 | 0 |
| `screen.rs` | 2,191 | 2,261 | 102 | 32 | +70 |
| `watch_replay.rs` | 338 | 338 | 0 | 0 | 0 |
| `widgets.rs` | 2,869 | 2,872 | 11 | 8 | +3 |
| **Total** | **12,350** | **12,407** | **1,497** | **1,440** | **+57** |

Affected test-only files contribute zero production lines and are omitted from
this ledger. `protocol.rs` and `watch_replay.rs` changed only in test-only lines.

### Growth versus deletion

This is not a net production reduction. The ledger reconciles as follows:

- `commands.rs` and `mod.rs`: **−92 net**. Legacy selection/control entry paths
  are deleted, although these files also gain reset guards and event-loop
  structure. This is a file subtotal, not a claim that precisely 92 lines belong
  exclusively to selection deletion.
- `app.rs`, `app/conversations.rs`, `screen.rs`, and `widgets.rs`: **+139 net**.
  Required responsibilities include the registry-reserved session-chat run row
  before discovery/first prompt, deduplicating that row against discovery,
  grouping its invocations, routing the authoritative run identity, preserving
  per-target drafts/transcripts, and reconciling/migrating render state when the
  session-chat identity changes. Reset/late-reply handling must not steal another
  conversation's selection or draft. These responsibilities explain growth;
  renaming the old target alone would not implement them. The acknowledgement
  fix distinguishes enqueue identity migration from session replacement: it
  preserves active/queued/completed turn identity, live execution, and pending
  receipt ownership instead of retiring the acknowledged turn. Together with
  its app entry point this adds 23 production lines; two additional blank lines
  around test hooks account for the remaining growth from +32 to +57.
- `client.rs`: **+1 net**; session-chat event module relocation: **+9 net**;
  session-chat client module relocation: **0 net** (responsibility descriptions).
- **−92 + 139 + 1 + 9 = +57**. Raw moves still contribute both sides to the
  1,497 additions and 1,440 deletions. The ledger does not invent a disjoint
  per-feature LOC budget for interleaved code.

### Separate test and documentation counts

The TUI test total includes **717 added lines** in `event_loop_tests.rs`, inline
unit tests/hooks, changed `entry_tests.rs`, and both sides of the external
session-chat client test-module relocations (descriptive, not historical labels). It excludes supervisor tests
outside the TUI prefix. Test growth is not counted as production growth.

The documentation row covers changed repository paths ending in `.md`, plus
changed `docs/*.py` audit scripts and `docs/exact-input-measurements/*.txt`
measurement tables (recursively), including untracked files.
It includes prompts and AGENTS files. It excludes this report itself, generated
cache files, Rust tests, and Rust production outside the TUI. Of its totals,
`crates/gremlins-cli/src/tui/AGENTS.md` alone is **+36/−19**. These are separate
scope totals, not a claim about all remaining repository changes.

## Requirement checklist and regression evidence

- [x] One conversation model: aggregate `Target::Run { run_id }` and exact
  `Target::Invocation { run_id, stage, stage_run_id }`. Separate session-chat
  destination kinds, legacy stage-execution targets, and `last_stage` selection
  state are absent from the active TUI Rust. Session chat is a registry-reserved
  aggregate run, not a separate destination kind. Tests:
  `app::tests::session_chat_run_row_is_distinct_from_its_invocation`,
  `app::tests::reserved_session_chat_run_is_listed_without_active_run_and_groups_children`.
- [x] Watch → Up/Down → Enter is the conversation-selection route. Highlight,
  inline pane toggles, and dismiss do not change the selected conversation.
  Enter opens aggregate run output or an exact invocation without pausing or
  binding native input. Tests:
  `entry_tests::watch_enter_exact_invocation_outside_retention_is_passive_and_isolates_drafts`,
  `entry_tests::watch_explicit_identity_never_selects_live_same_name_sibling`,
  `entry_tests::watch_rejection_closes_auxiliary_socket_without_attach_or_control_fallback`.
- [x] Remove alternate selectors `/attach`, `/debug`, and Ctrl+T, and remove
  `/exit`'s detach/return behavior. They are not active TUI selection commands;
  ordinary `/quit` remains local exit unless intercepted as a selected execution
  control. This does not remove the separate CLI exact-input interface. Tests:
  `entry_tests::selected_controls_reuse_native_handle_and_leaving_paused_sends_nothing`,
  `entry_tests::selected_exec_is_readonly_and_quit_outside_selection_is_local`.
- [x] Reserved row exists before the first run snapshot, discovery does not
  duplicate it, and authoritative enqueue identity installs before TurnStarted
  without stealing selection. Tests:
  `entry_tests::reserved_session_chat_row_is_enterable_before_first_run_snapshot`,
  `entry_tests::authoritative_replacement_while_browsing_does_not_steal_selection_or_draft`.
- [x] Reset is only valid on the session-chat aggregate; late success/failure
  cannot overwrite another selected composer's draft, model, or transcript.
  Tests:
  `entry_tests::reset_from_invocation_is_rejected_without_retiring_session_chat`,
  `entry_tests::delayed_reset_replies_preserve_selected_invocation_composer_model_and_transcript`,
  `app::conversations::tests::changing_session_chat_target_retires_only_its_generation`.
- [x] Background session-chat stream and selected passive watch remain separate;
  exact input binds lazily and receipts retain origin identity. Tests:
  `entry_tests::session_chat_background_stream_and_done_stay_in_session_chat_without_summary_duplicate`,
  `entry_tests::exact_watch_is_passive_first_input_is_a_and_bus_continues_once`,
  `entry_tests::delayed_selected_a_delivery_preserves_new_a_and_b_drafts`.

### Real event-loop coverage

`event_loop_tests.rs` drives crossterm key events through `run_app_with_io` into
the same `run_app_loop` used by production. It uses a ratatui `TestBackend`,
Unix socket pairs, and observed loop snapshots—not just direct helper calls.
All 11 tests passed:

- `enqueue_identity_installs_before_turn_started_without_stealing_b`
- `reset_aliases_on_b_are_rejected_without_clearing_the_composer`
- `late_successful_reset_reply_installs_reservation_without_touching_b`
- `late_failed_reset_reply_does_not_touch_b_draft`
- `failed_inline_follow_and_late_old_socket_do_not_change_selection_or_draft`
- `watch_highlight_toggle_and_dismiss_preserve_selected_draft`
- `repeated_enter_on_b_is_passive_and_restores_its_draft`
- `empty_discovery_watch_toggle_failure_and_dismiss_keep_reserved_draft`
- `removed_highlight_enter_keeps_reserved_draft_and_sends_no_input`
- `fresh_reservation_survives_leave_and_return_before_first_prompt`
- `reserved_session_row_returns_to_prompt_and_discovery_is_deduplicated`

Historical verification before the acknowledgement fix (not a final-checkout
test count):

```sh
cargo test -p gremlins-cli tui::event_loop_tests -- --nocapture
# 11 passed
cargo test -p gremlins-cli tui:: -- --quiet
# 546 passed, 0 failed (includes the 11 above)
```

No production code was changed to produce this report.

## Historical measurement labels

Historical rows are described by responsibility without retaining removed
destination terminology. Descriptive labels are not verbatim historical output
and do not imply that the current module names existed at the measured revision.
The before/after exact-input tables explicitly identify their descriptive
session-chat bus and identity test labels; their numeric measurements are
preserved. Historical evidence has no terminology exception.

## Reproduce

Run at the checkout root with Python 3 and Git. This is read-only apart from
throwaway temporary files. It imports the existing Rust lexer/brace matcher;
its `worktree()` helper is deliberately **not** used because that helper omits
untracked files. The diff here is of stripped streams, unlike that audit's
`measure()` helper, which classifies a diff of unstripped streams.

```sh
python3 - <<'PY'
import collections
import importlib.util
import pathlib
import subprocess
import tempfile

BASE = '784d4fca4d22ebc3e3538732fac91bdc0c849955'
PREFIX = 'crates/gremlins-cli/src/tui/'
REPORT = 'docs/watch-selection-measurements.md'
spec = importlib.util.spec_from_file_location('audit', 'docs/audit-plan11.py')
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)

def git(*args):
    return subprocess.check_output(['git', *args])

def paths(*args):
    return set(git(*args, '-z').decode().rstrip('\0').split('\0')) - {''}

old_paths = paths('ls-tree', '-r', '--name-only', BASE)
new_paths = {p for p in paths('ls-files', '--cached', '--others',
                            '--exclude-standard') if pathlib.Path(p).is_file()}

def load(ps, before):
    return {p: git('show', BASE + ':' + p) if before
            else pathlib.Path(p).read_bytes() for p in ps}

def diff(a, b):
    with tempfile.TemporaryDirectory() as d:
        x, y = pathlib.Path(d) / 'a', pathlib.Path(d) / 'b'
        x.write_bytes(a)
        y.write_bytes(b)
        r = subprocess.run(['git', 'diff', '--no-index', '--no-renames',
                            '--numstat', '--', str(x), str(y)],
                           capture_output=True, text=True)
        assert r.returncode in (0, 1), r.stderr
        return tuple(map(int, r.stdout.split()[:2])) if r.stdout else (0, 0)

old = load({p for p in old_paths if p.startswith(PREFIX) and p.endswith('.rs')}, True)
new = load({p for p in new_paths if p.startswith(PREFIX) and p.endswith('.rs')}, False)
ol, nl = audit.classify(old), audit.classify(new)

def streams(files, labels, p):
    lines = files.get(p, b'').decode().splitlines(keepends=True)
    flags = labels.get(p, [])
    return tuple(''.join(l for l, t in zip(lines, flags) if t == test).encode()
                 for test in (False, True))

total = collections.Counter()
affected = collections.Counter()
for p in sorted(old.keys() | new.keys()):
    a, at = streams(old, ol, p)
    b, bt = streams(new, nl, p)
    total.update(before=len(a.splitlines()), after=len(b.splitlines()))
    if old.get(p) == new.get(p):
        continue
    pa, pd = diff(a, b)
    ta, td = diff(at, bt)
    affected.update(before=len(a.splitlines()), after=len(b.splitlines()),
                    prod_add=pa, prod_del=pd, test_add=ta, test_del=td)
    print(p, len(a.splitlines()), len(b.splitlines()), pa, pd, ta, td)
print('all production', dict(total))
print('affected', dict(affected))

docs = collections.Counter()
for p in sorted(old_paths | new_paths):
    if p == REPORT or not (p.endswith('.md') or
                           (p.startswith('docs/') and p.endswith('.py')) or
                           (p.startswith('docs/exact-input-measurements/') and
                            p.endswith('.txt'))):
        continue
    a = git('show', BASE + ':' + p) if p in old_paths else b''
    b = pathlib.Path(p).read_bytes() if p in new_paths else b''
    if a != b:
        add, delete = diff(a, b)
        docs.update(add=add, delete=delete)
        print('doc', p, add, delete)
print('docs excluding report', dict(docs))
PY
```
