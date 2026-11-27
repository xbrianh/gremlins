# Home correction: base versus final diagnosis

## Scope and method

- Base: `9809155f8` (detached temporary worktree).
- Final: `b35543e34` (separate detached temporary worktree).
- Inspected final `crates/gremlins-cli/src/tui/entry_tests.rs:1545–1721`, especially the seven modes in `home_two_turn_completion_routes_fresh_prompts_and_refuses_stale_corrections`.
- No shared production source was edited for this diagnosis. `docs/diagnose-home-base.py` ports that final test to base's signatures in an isolated worktree and adds state logging there. It requires a separate worktree in the same repository, at the exact base HEAD, with both destination files clean; it refuses to overwrite a previous installation. Common directories use `rev-parse --path-format=absolute --git-common-dir`, avoiding caller-relative resolution.
- The refreshed port consumes input through production `app.take_input().resolve()` before `send_chat_input`, rather than synthesizing input from the draft. All seven modes and both downstream probes were rerun through this path.
- Targeted tests isolate the historical paths. Broader checks on the updated checkout are recorded separately below; they are not evidence about either immutable commit.

## Observed results

| Path | Actual base result | Actual final result |
| --- | --- | --- |
| Ordinary visible completion (`visible`) | **PASS**, ported final test. Terminal retires turn; new draft pins `IdleHome`; second request is fresh, untargeted `chat` with a new turn. | **PASS**, final seven-mode test. Reasoning completion, final answer and tool result render; second request is fresh untargeted `chat`. |
| Parked Home (`parked`) | **PASS**, ported final test. Completion while another conversation is selected preserves `unrelated draft`; returning Home permits fresh untargeted second chat. | **PASS**, same assertions. |
| Missing transcript (`missing-done`) | **FAIL**, first assertion after terminal: `missing-done: terminal did not retire turn`. See state and first transition below. | **PASS**. Terminal retirement no longer requires a display turn; second chat is fresh and untargeted. |
| Missing transcript (`missing-ended`) | **FAIL**, first assertion after terminal: `missing-ended: terminal did not retire turn`. Same state as `missing-done`. | **PASS**, including fresh untargeted second chat. |
| Late frames after second submission | **PASS** in base port's visible, parked and failure modes: old turn `1` `done`, `ended`, and `error` leave active turn `2`; third draft waits for a writable execution, without stale-address warning or fallback. Separate original base late-terminal test also passes in `home_` baseline. Missing modes stop earlier, so their late-frame suffix was **not reached** on base. | **PASS** in visible, parked, missing-done, missing-ended and failure modes. Correction modes take an earlier branch and do not exercise this suffix. |
| Pre-existing exact correction (`correction`) | **PASS**: draft stays pinned to `home/exec-A` after retirement; Enter refuses it with `addressed execution has changed`; explicit dismissal permits a fresh prompt. | **PASS**, same behavior. |
| Pre-stage failure (`failure`) | **PASS**: failure retires turn, error renders, fresh second chat succeeds. | **PASS**. |
| Draft during startup (`starting-correction`) | **PASS**: draft pinned to `StartingHome("1")` remains a stale correction after failure; explicit dismissal releases it. | **PASS**. |

Ordinary, parked, late-frame isolation and stale-correction rejection are **passing baselines**, not reproduced base bugs. Only the missing-transcript retirement paths above failed in this comparison. This is a controlled display-gap reproduction (`clear_current_turn()`), not proof of a specific real-world cause for losing the transcript.

## Missing transcript: FIRST failed transition on base

Sequence: submit first prompt through the real prompt entry point, acknowledge chat/model requests, receive an identity-bearing reasoning frame and tool result, call `live.clear_current_turn()`, then deliver a matching-turn `done` or `ended` without execution identity.

Expected first transition: matching terminal retires Home turn `1` and clears the live execution regardless of whether its transcript still has a current turn.

Observed immediately after either terminal, **before composing a second prompt**:

```text
selected=Home
active=Some("1")
display_turn=false
active_request=true
pinned=None
current=Execution(Execution { run_id: "home", stage_run_id: "exec-A", nonce: 23 })
starting=Some("1")
latest=Some("1")
```

There is **no pinned draft at this first failure**: the first prompt was consumed and no second prompt exists yet. The current routing address nevertheless remains `home/exec-A`, and the controller's current turn remains `1`, while the display has no current turn. Base `home_events.rs:189–192` (`Done`) and `217–219` (`Ended`) return early on the missing display turn, before `finish_home_request` and clearing `active_turn`.

A separate downstream-only diagnostic continues past that known failure and composes `second prompt`:

```text
selected=Home
active=Some("1")
pinned=Some(Execution(Execution { run_id: "home", stage_run_id: "exec-A", nonce: 23 }))
current=Execution(Execution { run_id: "home", stage_run_id: "exec-A", nonce: 23 })
submission=Correction { id: 3, execution: Execution { run_id: "home", stage_run_id: "exec-A", nonce: 23 }, text: "second prompt" }
```

Thus this fixture does **not** first fail with a pinned/current-address mismatch or the stale-address warning. The first failure is missed retirement; downstream the newly composed draft pins the still-current old execution and is classified as a correction. That probe calls the conversation classifier only; it does not claim an actual correction was delivered. Its successful test exit means diagnostic completion, **not** that missing-transcript behavior passes on base.

## Executed checks

- Unmodified base: `cargo test -q -p gremlins-cli home_ -- --nocapture`: **52 passed**, no failures (other binaries had zero matching tests).
- Refreshed base port using production `take_input().resolve()`, one mode per process via the supplemental make target below: visible, parked, correction, failure, starting-correction each **passed** (1 test, 585 filtered out). Missing-done and missing-ended each **failed** (1 test), at `terminal did not retire turn`; Cargo exit `101`, make exit `2`. A dedicated `.diagnostic-refresh-target` prevented shared-target collisions.
- Both refreshed downstream-only missing probes **completed** (1 test each, make exit `0`) and logged `Submission::Correction` as above. These bypass the known retirement assertion, not the failure itself.
- Safety checks: shared checkout, modified destination files, and wrong HEAD each refused with script exit `1`. A clean same-repository base worktree was accepted. Cross-repository refusal is implemented but was not separately exercised.
- This refresh ran neither `make test` nor `make check`; it made no shared Rust or Makefile edits.
- Exact final detached worktree: `cargo test -q -p gremlins-cli --bin gremlins home_ -- --nocapture`: **50 passed**, no failures, including the seven-mode test.
- Initial shared final checkout targeted seven-mode run: **1 passed**. Later shared checkout `home_` run with a separate target: **52 passed**; this checkout had parent-owned changes, so use the **50-test detached final result** for the exact commit comparison.
- One intervening shared-target rerun executed a stale base diagnostic binary (52 passes, 1 expected diagnostic failure). It is **excluded** as final evidence. Separate target directories avoid this Cargo artifact collision.

## Review-addressing validation (current working tree)

- `make fmt`: **PASS**.
- `make test-cli`: binary suite **584 passed, one ignored**, including identity,
  interrupt and eight-mode two-turn regressions. The command then failed in
  `tests/artifact_exists.rs`: six integration cases could not spawn the required
  executable (`No such file or directory`). This is not a passing full CLI run.
- The eight-mode fixture now covers a real initial socket enqueue rejection,
  consumes the client-produced failure, preserves its queued correction, refuses
  stale Enter, then sends a fresh prompt after explicit dismissal. Every mode
  checks exact second turn `"2"` and absence of target/stage execution fields.
- `make check` and `make test` were deliberately left to the dedicated verifier.

## Historical updated-checkout validation

Historical results recorded before this diagnostic refresh, after the Ctrl+G fix and its two regression tests (not rerun here):

- `make check`: **PASS** (format, Clippy and Cargo check).
- `make test`: **PASS**, 1,934 library tests and 49 landscape tests.
- `make test-cli CARGO_TARGET_DIR="$PWD/.parent-final-target"`: **PASS**, 584 binary tests, one ignored, plus 36 integration tests across five suites.
- The first `make test-cli` using the shared default target executed the stale base diagnostic binary and failed. That run is excluded as checkout evidence; the separate-target run above compiled and tested the actual updated sources.
- `python3 docs/home-correction-reduction.py` and `git diff --check`: **PASS**.

## Reproduce

Run from repository root. Use fresh, separate targets for base and final. Temporary worktrees may live elsewhere if permitted by your environment. The repository `test-cli` recipe has no `TEST`/`TEST_FILTER` or extra Cargo-flags hook; passing those variables would not filter it. Use a temporary supplemental make target instead, without editing `Makefile`. An empty `TEST_FILTER` runs all binary tests.

```sh
ROOT="$PWD"
FILTER_MK=$(mktemp "$ROOT/.diagnostic-filter.XXXXXX")
# printf supplies the recipe's required literal tab.
printf '.PHONY: test-cli-filtered\ntest-cli-filtered:\n\tcargo test -q -p gremlins-cli --bin gremlins $(TEST_FILTER) -- --nocapture\n' > "$FILTER_MK"
git worktree add --detach .diagnostic-base-9809155f8 9809155f8
git worktree add --detach .diagnostic-final-b35543e34 b35543e34

# Unchanged base baseline, before installing diagnostics.
(cd .diagnostic-base-9809155f8 && \
  make -f Makefile -f "$FILTER_MK" test-cli-filtered TEST_FILTER=home_ \
    CARGO_TARGET_DIR="$ROOT/.diagnostic-base-target")

python3 docs/diagnose-home-base.py .diagnostic-base-9809155f8
for mode in visible parked missing-done missing-ended correction failure starting-correction; do
  (cd .diagnostic-base-9809155f8 && \
    HOME_DIAGNOSTIC_MODE="$mode" \
    make -f Makefile -f "$FILTER_MK" test-cli-filtered TEST_FILTER=diagnostic_home_two_turn \
      CARGO_TARGET_DIR="$ROOT/.diagnostic-base-target")
  echo "$mode make_exit=$?"  # missing modes: make 2, Cargo 101; continue
 done

# Inspect downstream classification without treating it as a passing regression test.
for mode in missing-done missing-ended; do
  (cd .diagnostic-base-9809155f8 && \
    HOME_DIAGNOSTIC_MODE="$mode" HOME_DIAGNOSTIC_AFTER_FAILURE=1 \
    make -f Makefile -f "$FILTER_MK" test-cli-filtered TEST_FILTER=diagnostic_home_two_turn \
      CARGO_TARGET_DIR="$ROOT/.diagnostic-base-target")
done

(cd .diagnostic-final-b35543e34 && \
  make -f Makefile -f "$FILTER_MK" test-cli-filtered TEST_FILTER=home_ \
    CARGO_TARGET_DIR="$ROOT/.diagnostic-final-target")

git worktree remove --force .diagnostic-base-9809155f8
git worktree remove .diagnostic-final-b35543e34
rm "$FILTER_MK"
```

During the original run base used the existing root `target`; the separate base target above is an intentional reproducibility safeguard. The diagnostic script changes only the isolated worktree's test file and adds a logging helper to its conversation module. Neither change belongs in production. To reinstall, restore only those two disposable-worktree files first; the safety check intentionally rejects dirty destinations.
