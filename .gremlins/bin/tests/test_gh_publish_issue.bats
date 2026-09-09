#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_publish_issue"
    PLAN_FILE="$(mktemp)"
}

teardown() {
    teardown_mocks
    rm -f "$PLAN_FILE"
}

@test "creates issue from plan file and outputs number" {
    printf '# My Feature Title\n\nPlan body here\n' > "$PLAN_FILE"
    mock_gh 'issue create.*--body-file.*--title My Feature Title' 'https://github.com/owner/repo/issues/55'
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 0 ]
    [ "$output" = "55" ]
}

@test "dies when plan file is empty" {
    printf '' > "$PLAN_FILE"
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 1 ]
    [[ "$output" == *"empty"* ]]
}

@test "dies when plan file has no H1" {
    printf 'no heading here\n' > "$PLAN_FILE"
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 1 ]
    [[ "$output" == *"no H1"* ]]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}
