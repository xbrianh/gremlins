# Branch Review: `overlays` vs `main`

## Summary

The `overlays` branch extracts inline shell scripts from `.gremlins/stages/*.yaml` pipeline YAML files into standalone, tested shell scripts under `.gremlins/bin/`. Each script gets dedicated bats tests with a custom mocking framework. This is a substantial quality improvement — the YAML stages become thin wrappers around independently testable scripts.

**Stats:** 48 files changed, +1554 / −236 lines.

---

## What changed

### New: `.gremlins/bin/` — standalone shell scripts

| Script | Purpose |
|---|---|
| `gh_ci_await` | Wait for CI check-runs to appear (startup grace) or SHA sync post-fix |
| `gh_ci_poll` | Poll until all CI checks complete, output `passed`/`failed` |
| `gh_ci_failure_log` | Download failure logs for all failed CI checks |
| `gh_create_pr` | Push branch, create PR, write number + URL |
| `gh_discover_repo` | Discover `owner/repo` from the current directory |
| `gh_gather_review_content` | Gather all PR review content into markdown |
| `gh_publish_issue` | Publish a plan markdown file as a GitHub issue |
| `gh_push_to_pr_branch` | Push HEAD to a PR branch |
| `gh_request_reviewer` | Request Copilot review on a PR |
| `gh_resolve_plan_source` | Resolve a plan source reference (`#N` or `owner/repo#N`) |
| `gh_wait_copilot` | Poll for Copilot review completion |
| `git_diff_summary` | Compute diff summary between base ref and HEAD |
| `gh_wait_ci` | **(still present, see issue below)** Monolithic CI wait — all-in-one |

### New: `.gremlins/bin/lib/` — shared libraries

| Library | Contents |
|---|---|
| `logging.sh` | `die`, `info`, `warn`, `bail` helpers |
| `git.sh` | `push_branch` helper |
| `polling.sh` | `poll_until` helper **(unused — see issue below)** |

### New: `.gremlins/bin/tests/` — bats test suite

- `helpers/mocks.bash` — mocking framework for `gh`, `git`, and arbitrary commands
- Test files for each script: `test_gh_create_pr.bats`, `test_gh_discover_repo.bats`, `test_gh_gather_review_content.bats`, `test_gh_publish_issue.bats`, `test_gh_push_to_pr_branch.bats`, `test_gh_request_reviewer.bats`, `test_gh_resolve_plan_source.bats`, `test_gh_wait_ci.bats`, `test_gh_wait_copilot.bats`, `test_git_diff_summary.bats`
- JSON/binary fixtures for mocking `gh` CLI output

### Modified: pipeline YAMLs

- **`.gremlins/gh.yaml`** — `bootstrap.env` adds `.gremlins/bin` to `PATH`; `gather-github-review-content` delegates to `gh_gather_review_content`
- **`.gremlins/stages/github-discover-repo.yaml`** — delegates to `gh_discover_repo`
- **`.gremlins/stages/github-open-pr.yaml`** — `compute-diff` delegates to `git_diff_summary`; `commit-pr` delegates to `gh_create_pr`
- **`.gremlins/stages/github-request-copilot-review.yaml`** — delegates to `gh_request_reviewer`
- **`.gremlins/stages/github-wait-copilot.yaml`** — delegates to `gh_wait_copilot` (one-shot, no internal retry; retry is handled by the outer loop)
- **`.gremlins/stages/github-wait-ci.yaml`** — major restructure: monolithic `poll` stage split into `ci-await` → `ci-poll` → `ci-failure-log` stages, each delegating to a separate script
- **`.gremlins/stages/plan-gh.yaml`** — `resolve-source` delegates to `gh_resolve_plan_source`; `publish-as-issue` delegates to `gh_publish_issue`

### Modified: CI workflow & Makefile

- **`.github/workflows/ci.yml`** — new job step: `apt-get install bats && make test-github-integration-scripts`
- **`Makefile`** — new target `test-github-integration-scripts` that runs `bats .gremlins/bin/tests/`

---

## Issues found

### 1. Dead code: `gh_wait_ci` (monolithic script) is unused

**File:** `.gremlins/bin/gh_wait_ci`

The monolithic `gh_wait_ci` script (207 lines) is still present but **no longer called by any pipeline YAML**. The `github-wait-ci` stage now uses `gh_ci_await`, `gh_ci_poll`, and `gh_ci_failure_log` separately. The only reference to `gh_wait_ci` outside itself is its bats test file (`test_gh_wait_ci.bats`).

**Severity:** Medium. Dead code with maintenance burden. Either remove it (and its test), or if it's intended as a convenience wrapper, document why it lives alongside the split scripts.

**Recommendation:** Remove `gh_wait_ci` and `test_gh_wait_ci.bats`.

---

### 2. Dead code: `polling.sh` is never sourced

**File:** `.gremlins/bin/lib/polling.sh`

The `poll_until` function defined here is not imported or used by any script in `.gremlins/bin/`. The individual scripts (e.g., `gh_ci_poll`, `gh_wait_copilot`) implement their own polling loops inline rather than using this helper.

**Severity:** Low. Adds zero functionality but also zero harm. The function is well-written and could be a useful abstraction if the scripts were refactored to use it.

**Recommendation:** Either use it or remove it. If the intent is future refactoring, add a comment.

---

### 3. Dead mock in `test_gh_create_pr.bats`

**File:** `.gremlins/bin/tests/test_gh_create_pr.bats`, line in `@test "creates PR and writes number and URL files"`

```bash
mock_cmd 'python3' '.*random.*choices.*' 'abcd'
```

`gh_create_pr` generates its random slug via bash built-in `printf '%04x' "${RANDOM}"` — it never calls `python3`. This mock is never consumed. It appears to be a leftover from a prior iteration of the script.

**Severity:** Low. Harmless but misleading. Someone reading the test might assume `gh_create_pr` depends on python3.

**Recommendation:** Remove the unused mock line.

---

### 4. SHA-sync timeout loss in `gh_ci_await`

**File:** `.gremlins/bin/gh_ci_await`, lines 67–79

In the original monolithic `gh_wait_ci`, the post-fix SHA sync loop had a `push_deadline` timeout with a specific error message: `"ci-gate: timed out waiting for GitHub to reflect pushed SHA"`. In `gh_ci_await`, the SHA sync loop polls indefinitely — there is no internal timeout. The YAML stage has `timeout: 180`, so the process will eventually be killed by the orchestrator, but the bail message will be generic rather than the specific SHA-sync message.

**Severity:** Low. The effective behavior is similar (bail after ~180s), but the diagnostic message is less specific. The fix-head bail file won't be written with a clear reason.

**Recommendation:** Restore a timeout (or at least a bail message) in the SHA-sync loop. Alternatively, accept that the stage-level `timeout` is sufficient and document the behavior.

---

### 5. `gh_wait_copilot` called one-shot via outer loop

**File:** `.gremlins/stages/github-wait-copilot.yaml`

```yaml
cmds:
  - 'output=$(gh_wait_copilot "{pr_number}" "{repo}" 2>/dev/null); if [ -n "$output" ]; then printf "%s\n" "$output" > "{status}"; fi'
```

The `gh_wait_copilot` script has been simplified to a one-shot check that takes only two arguments (`pr_number`, `repo`). The retry loop is handled by the outer `loop` stage. The script has no internal retry capability, so there is no dead code.

**Severity:** N/A — the script and pipeline are aligned.

**Recommendation:** None.

---

### 6. Missing trailing newlines

**Files:** `gh.yaml`, `github-wait-ci.yaml`, `gh_ci_poll`, `gh_ci_await`, `gh_ci_failure_log`, `gh_create_pr`, `gh_resolve_plan_source`, `gh_wait_ci`, and all test/mock files.

POSIX defines a line as ending with `\n`. Several scripts lack a final newline. bash handles this gracefully, but it's inconsistent with the project's existing conventions and can cause issues with some tools (e.g., `wc -l`, `sed`).

**Severity:** Low / cosmetic.

**Recommendation:** Add trailing newlines consistently.

---

### 7. `gh_ci_poll` calls `gh pr view` 3 times per poll iteration

**File:** `.gremlins/bin/gh_ci_poll`

Each iteration of the poll loop calls:
1. `fetch_meta()` — one `gh pr view`
2. `all_checks_done()` — a second `gh pr view`
3. After the loop: `count_failed()` — a third `gh pr view`

The original monolithic `gh_wait_ci` had the same pattern, so this isn't a regression, but it's 3 API calls per 30s where 1 would suffice (`fetch_meta` and `all_checks_done` fetch the same `statusCheckRollup` data). For context, `gh_wait_ci` used `get_failed_count` which calls `gh` once and returns both count and the JSON.

**Severity:** Low. API rate limits are generous (5,000/hr). At 30s intervals this is ~360 calls/hr total across all 3 invocations.

**Recommendation:** Consider merging `fetch_meta` + `all_checks_done` into a single `gh pr view` call, as `gh_wait_ci` does.

---

## What's good

- **Testability:** Every GitHub operation can now be tested without network calls. The mocking framework in `mocks.bash` is elegant — pattern-based dispatch with sequential consumption, so repeated calls with the same pattern can return different data.
- **Separation of concerns:** Pipeline YAML is now a thin orchestration layer; the actual logic lives in testable scripts.
- **CI coverage:** The `ci.yml` workflow installs bats and runs the integration script tests on every push.
- **Error handling:** Scripts use `set -euo pipefail` consistently and propagate bail messages via stderr + exit 2.
- **Library reuse:** `logging.sh` is sourced by every script, providing consistent `die`/`info`/`warn` behavior.
- **Fix-head capture improvement:** The old pipeline wrote `fix-head` to `$GREMLINS_ARTIFACT_DIR/fix-head` (a hardcoded path). The new pipeline uses `bind: fix_head: "artifact://fix-head"` and `git rev-parse HEAD > "{fix_head}"`, properly scoping the artifact through the artifact system.