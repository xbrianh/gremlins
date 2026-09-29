#!/usr/bin/env bats
#
# Tests for ci_wait — the await-then-poll CI orchestration script.

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../ci_wait"
    WORK="$(mktemp -d /tmp/ci_wait_test.XXXXXX)"
    DONE="${WORK}/done"
    STATUS="${WORK}/status"
    PR_URL="https://github.com/owner/repo/pull/42"
}

teardown() {
    teardown_mocks
    rm -rf "$WORK"
}

# --- helpers ---

mk_mock() {
    mock_cmd "$1" '.*' "$2" "${3:-0}"
}

# --- await: skipped ---

@test "await: touches done when gh_ci_await outputs 'skipped'" {
    mk_mock gh_ci_await "ci-gate: no check-runs after 60s grace; skipping
skipped"
    run bash "$SCRIPT" "$PR_URL" "" "$DONE" "$STATUS"
    [ "$status" -eq 0 ]
    [ -f "$DONE" ]
}

# --- await: already passed ---

@test "await: touches done when CI already passed" {
    mk_mock gh_ci_await "ci-gate: check typename=CheckRun name=ci/test status=COMPLETED conclusion=SUCCESS"
    run bash "$SCRIPT" "$PR_URL" "" "$DONE" "$STATUS"
    [ "$status" -eq 0 ]
    [ -f "$DONE" ]
}

# --- await → poll: passed ---

@test "await+poll: touches done and writes status when poll returns 'passed'" {
    mk_mock gh_ci_await "ci-gate: check typename=CheckRun name=ci/test status=IN_PROGRESS conclusion=N/A"
    mk_mock gh_ci_poll $'ci-gate: polling...\npassed'
    run bash "$SCRIPT" "$PR_URL" "" "$DONE" "$STATUS" 60 30 180 5
    [ "$status" -eq 0 ]
    [ -f "$DONE" ]
    [ "$(tail -n 1 "$STATUS")" = "passed" ]
}

# --- await → poll: failed ---

@test "await+poll: writes status but does NOT touch done when poll returns 'failed'" {
    mk_mock gh_ci_await "ci-gate: check typename=CheckRun name=ci/test status=IN_PROGRESS conclusion=N/A"
    mk_mock gh_ci_poll $'ci-gate: polling...\nfailed'
    run bash "$SCRIPT" "$PR_URL" "" "$DONE" "$STATUS" 60 30 180 5
    [ "$status" -eq 0 ]
    [ ! -f "$DONE" ]
    [ "$(tail -n 1 "$STATUS")" = "failed" ]
}

# --- error propagation ---

@test "exits 2 when gh_ci_await fails" {
    mk_mock gh_ci_await "timeout" 2
    run bash "$SCRIPT" "$PR_URL" "" "$DONE" "$STATUS"
    [ "$status" -eq 2 ]
}

@test "exits 2 when gh_ci_poll fails" {
    mk_mock gh_ci_await "ci-gate: check typename=CheckRun name=ci/test status=IN_PROGRESS conclusion=N/A"
    mk_mock gh_ci_poll "timeout" 2
    run bash "$SCRIPT" "$PR_URL" "" "$DONE" "$STATUS" 60 30 180 5
    [ "$status" -eq 2 ]
}

# --- usage ---

@test "dies without required arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}