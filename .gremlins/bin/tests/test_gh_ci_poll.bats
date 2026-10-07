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
        --json) cat '$fixture'; exit 0;;
        *) shift;;
    esac
done
jq -r "\$jqf" '$fixture'
GHMOCK
    chmod +x "$MOCK_DIR/gh"
}

# create_stateful_gh_mock returns $1 on the first --json call and $2 on all
# subsequent calls.  Used to test scripts that loop until conditions change.
create_stateful_gh_mock() {
    local first_fixture="$1"
    local second_fixture="$2"
    local counter="$MOCK_DIR/.gh_call_count"
    echo 0 > "$counter"
    cat > "$MOCK_DIR/gh" <<GHMOCK
#!/usr/bin/env bash
set -euo pipefail
count=\$(cat '$counter')
echo \$(( count + 1 )) > '$counter'
jqf=""
while [[ \$# -gt 0 ]]; do
    case "\$1" in
        --jq) jqf="\$2"; shift 2;;
        --json)
            if [ "\$count" -eq 0 ]; then
                cat '$first_fixture'
            else
                cat '$second_fixture'
            fi
            exit 0
            ;;
        *) shift;;
    esac
done
if [ "\$count" -eq 0 ]; then
    jq -r "\$jqf" '$first_fixture'
else
    jq -r "\$jqf" '$second_fixture'
fi
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

@test "loops until statusCheckRollup is populated, then passes" {
    create_stateful_gh_mock "$FIXTURES/ci_rollup_no_checks.json" "$FIXTURES/ci_rollup_passed.json"
    run bash "$SCRIPT" "$PR_URL" 0
    [ "$status" -eq 0 ]
    [ "${output##*$'\n'}" = "passed" ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}