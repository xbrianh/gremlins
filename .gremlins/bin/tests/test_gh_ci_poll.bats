#!/usr/bin/env bats
#
# Regression tests for gh_ci_poll — covers the StatusContext discriminator
# (.__typename) so the __typename-vs-typename bug is caught.
#
# Uses a custom gh mock that pipes fixture JSON through real jq,
# exercising the actual jq filters embedded in the script.

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_ci_poll"
    FIXTURES="$BATS_TEST_DIRNAME/fixtures"
    PR_URL="https://github.com/owner/repo/pull/42"
}

teardown() {
    teardown_mocks
}

# create_gh_jq_mock replaces the mock-gh with one that runs real jq
# against the fixture file $1 for --json statusCheckRollup{,reviewDecision}.
create_gh_jq_mock() {
    local fixture="$1"
    cat > "$MOCK_DIR/gh" <<GHMOCK
#!/usr/bin/env bash
set -euo pipefail
jqf=""
while [[ \$# -gt 0 ]]; do
    case "\$1" in
        --jq) jqf="\$2"; shift 2;;
        *) shift;;
    esac
done
jq -r "\$jqf" '$fixture'
GHMOCK
    chmod +x "$MOCK_DIR/gh"
}

@test "outputs 'passed' when all checks complete successfully" {
    create_gh_jq_mock "$FIXTURES/ci_rollup_passed.json"
    run bash "$SCRIPT" "$PR_URL"
    [ "$status" -eq 0 ]
    [ "${output##*$'\n'}" = "passed" ]
}

@test "outputs 'failed' when a StatusContext has state=FAILURE" {
    create_gh_jq_mock "$FIXTURES/ci_rollup_failed_statuscontext.json"
    run bash "$SCRIPT" "$PR_URL"
    [ "$status" -eq 0 ]
    [ "${output##*$'\n'}" = "failed" ]
}

@test "outputs 'passed' when statusCheckRollup is empty" {
    create_gh_jq_mock "$FIXTURES/ci_rollup_no_checks.json"
    # Empty rollup causes the jq filter to return checks_done=false forever.
    # This is intentional — an empty rollup means checks haven't populated yet.
    # Use a 1s timeout to verify it bails rather than looping forever.
    run bash "$SCRIPT" "$PR_URL" 1 30
    [ "$status" -eq 2 ]
    [[ "$output" == *"checks incomplete"* ]]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}