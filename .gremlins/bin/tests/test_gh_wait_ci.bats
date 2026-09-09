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

# Helper: add a mock for fetch_status (gh pr view + python3 pipe).
# Returns the decision, rollup_len, and headRefOid as 3 newline-separated values.
mock_fetch_status() {
    local json="$1"
    local output="$2"
    mock_gh 'pr view.*statusCheckRollup,reviewDecision,headRefOid' "$json"
    mock_cmd 'python3' 'print.*len.*rollup' "$output"
}

# Helper: add a mock for all_checks_done (gh pr view + python3 pipe).
mock_all_checks_done() {
    local json="$1"
    local output="$2"
    mock_gh 'pr view.*--json statusCheckRollup$' "$json"
    mock_cmd 'python3' 'for item in rollup' "$output"
}

# Helper: add a mock for get_failed_checks.
mock_get_failed_checks() {
    local json="$1"
    local output="$2"
    mock_gh 'pr view.*--json statusCheckRollup$' "$json"
    mock_cmd 'python3' 'FAILURE.*ERROR.*TIMED_OUT.*CANCELLED' "$output"
}

@test "all checks passed writes passed to status and done" {
    PASS_JSON='{"statusCheckRollup":[{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"SUCCESS"}],"reviewDecision":null,"headRefOid":"abc1234"}'

    # Grace period: fetch_status
    mock_fetch_status "$PASS_JSON" $'\n1\nabc1234'
    # Poll loop: all_checks_done → "true"
    mock_all_checks_done "$PASS_JSON" 'true'
    # Poll loop: fetch_status (called before break check in loop body)
    mock_fetch_status "$PASS_JSON" $'\n1\nabc1234'
    # After loop: get_failed_checks
    mock_get_failed_checks "$PASS_JSON" $'0\n'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 0 ]
    [ "$(cat "$ARTIFACT_DIR/status")" = "passed" ]
    [ -f "$ARTIFACT_DIR/ci_poll_done" ]
}

@test "skipped when no check-runs after grace period" {
    EMPTY_JSON='{"statusCheckRollup":[],"reviewDecision":null,"headRefOid":"abc1234"}'
    mock_fetch_status "$EMPTY_JSON" $'\n0\nabc1234'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 0 ]
    [ "$(cat "$ARTIFACT_DIR/status")" = "skipped" ]
}

@test "bails on REVIEW_REQUIRED" {
    RR_JSON='{"statusCheckRollup":[],"reviewDecision":"REVIEW_REQUIRED","headRefOid":"abc1234"}'
    mock_fetch_status "$RR_JSON" $'REVIEW_REQUIRED\n0\nabc1234'

    run bash "$SCRIPT" "$ARTIFACT_DIR" "$PR_URL" 0 30 1
    [ "$status" -eq 2 ]
    [ -f "$ARTIFACT_DIR/ci_bail" ]
}

@test "failure collects logs and exits 1" {
    FAIL_JSON='{"statusCheckRollup":[{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://github.com/owner/repo/actions/runs/2"}],"reviewDecision":null,"headRefOid":"abc1234"}'

    # Grace period: fetch_status
    mock_fetch_status "$FAIL_JSON" $'\n1\nabc1234'
    # Poll loop: all_checks_done → "true"
    mock_all_checks_done "$FAIL_JSON" 'true'
    # Poll loop: fetch_status
    mock_fetch_status "$FAIL_JSON" $'\n1\nabc1234'
    # After loop: get_failed_checks → 1 failure
    mock_get_failed_checks "$FAIL_JSON" $'1\n[{"name":"test","conclusion":"FAILURE","detailsUrl":"https://github.com/owner/repo/actions/runs/2"}]'
    # Failure log formatting
    mock_cmd 'python3' 'failed = json.load' $'\n## Check: test\n\n(gh run view 2 --log-failed)\n'

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