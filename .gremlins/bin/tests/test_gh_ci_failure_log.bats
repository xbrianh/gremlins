#!/usr/bin/env bats
#
# Regression tests for gh_ci_failure_log — covers the StatusContext
# discriminator (.__typename) so the __typename-vs-typename bug is caught.
#
# Uses a custom gh mock that pipes fixture JSON through real jq,
# exercising the _FAILED_FILTER embedded in the script.

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_ci_failure_log"
    FIXTURES="$BATS_TEST_DIRNAME/fixtures"
    PR_URL="https://github.com/owner/repo/pull/42"
}

teardown() {
    teardown_mocks
}

# create_gh_jq_mock replaces the mock-gh with one that:
#   - pipes fixture JSON through real jq for "pr view" calls
#   - returns canned log text for "run view" calls (detected by $2=run,$3=view)
create_gh_jq_mock() {
    local fixture="$1"
    local run_log="${2:-fake log}"
    cat > "$MOCK_DIR/gh" <<GHMOCK
#!/usr/bin/env bash
set -euo pipefail
if [ "\$2" = "run" ] && [ "\$3" = "view" ]; then
    printf '%s\n' '$run_log'
    exit 0
fi
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

@test "outputs failure details for a failed StatusContext" {
    create_gh_jq_mock "$FIXTURES/ci_rollup_failed_statuscontext.json"
    run bash "$SCRIPT" "$PR_URL"
    [ "$status" -eq 0 ]
    [[ "$output" == *"ci/test"* ]]
}

@test "outputs nothing when there are no failures" {
    create_gh_jq_mock "$FIXTURES/ci_rollup_passed.json"
    run bash -c '"$@" 2>/dev/null' _ "$SCRIPT" "$PR_URL"
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}