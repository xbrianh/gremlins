#!/usr/bin/env bats
#
# Tests for ci_failure_log — thin wrapper around gh_ci_failure_log.

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../ci_failure_log"
    WORK="$(mktemp -d /tmp/ci_failure_log_test.XXXXXX)"
    OUT="${WORK}/ci_failure.txt"
    PR_URL="https://github.com/owner/repo/pull/42"
}

teardown() {
    teardown_mocks
    rm -rf "$WORK"
}

@test "writes gh_ci_failure_log output to the specified file" {
    mock_cmd gh_ci_failure_log '.*' "## Check: ci/test
(gh run view 123 --log-failed)
fake log output"

    run bash "$SCRIPT" "$PR_URL" "$OUT"
    [ "$status" -eq 0 ]
    [ -f "$OUT" ]
    grep -q "fake log output" "$OUT"
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}