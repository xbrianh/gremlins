#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_wait_ci"
    ARTIFACT_DIR="$(mktemp -d)"
}

teardown() {
    teardown_mocks
    rm -rf "$ARTIFACT_DIR"
}

PR_URL="https://github.com/owner/repo/pull/1"

# Helper: add a mock for fetch_meta (gh pr view --jq).
# Returns the decision, rollup_len, and headRefOid as 3 newline-separated values.
mock_fetch_meta() {
    local output="$1"
    mock_gh 'pr view.*statusCheckRollup,reviewDecision,headRefOid' "$output"
}

# Helper: add a mock for all_checks_done (gh pr view --jq).
mock_all_checks_done() {
    local output="$1"
    mock_gh 'pr view.*--json statusCheckRollup' "$output"
}

# Helper: add a mock for get_failed_count (gh pr view --jq).
mock_get_failed_count() {
    local output="$1"
    mock_gh 'pr view.*--json statusCheckRollup' "$output"
}

@test "all checks passed writes passed to status and done" {
    # Grace period: fetch_meta
    mock_fetch_meta $'\n1\nabc1234'
    # Poll loop: all_checks_done → "true"
    mock_all_checks_done 'true'
    # Poll loop: fetch_meta (called before break check in loop body)
    mock_fetch_meta $'\n1\nabc1234'
    # After loop: get_failed_count → 0
    mock_get_failed_count $'0\n'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 0 ]
    [ "$(cat "$ARTIFACT_DIR/status")" = "passed" ]
    [ -f "$ARTIFACT_DIR/ci_poll_done" ]
}

@test "skipped when no check-runs after grace period" {
    mock_fetch_meta $'\n0\nabc1234'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 0 ]
    [ "$(cat "$ARTIFACT_DIR/status")" = "skipped" ]
}

@test "bails on REVIEW_REQUIRED" {
    mock_fetch_meta $'REVIEW_REQUIRED\n0\nabc1234'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 2 ]
    [ -f "$ARTIFACT_DIR/ci_bail" ]
}

@test "failure collects logs and exits 1" {
    FAILED_CHECKS_JSON='[{"name":"test","conclusion":"FAILURE","detailsUrl":"https://github.com/owner/repo/actions/runs/2"}]'

    # Grace period: fetch_meta
    mock_fetch_meta $'\n1\nabc1234'
    # Poll loop: all_checks_done → "true"
    mock_all_checks_done 'true'
    # Poll loop: fetch_meta
    mock_fetch_meta $'\n1\nabc1234'
    # After loop: get_failed_count → 1 failure + json
    mock_get_failed_count $'1\n[{"name":"test","conclusion":"FAILURE","detailsUrl":"https://github.com/owner/repo/actions/runs/2"}]'
    # Failure log formatting: gh pr view --jq _FAILED_PRINT_FILTER
    mock_gh 'pr view.*--json statusCheckRollup' $'\n## Check: test\n\n(gh run view 2 --log-failed)\n'
    # Failure log formatting: gh pr view --jq _FAILED_RUN_FILTER → run ID
    mock_gh 'pr view.*--json statusCheckRollup' $'2\n'
    # Fetch actual logs
    mock_gh 'run view 2 --log-failed' 'FAILURE: test failed: expected 42, got 0'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 1 ]
    [ "$(cat "$ARTIFACT_DIR/status")" = "failed" ]
    [ -f "$ARTIFACT_DIR/ci_failure.txt" ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}