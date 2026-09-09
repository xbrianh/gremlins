#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_request_reviewer"
}

teardown() {
    teardown_mocks
}

@test "adds copilot reviewer to PR" {
    mock_gh 'pr edit.*--add-reviewer copilot-pull-request-reviewer' '' 0
    run bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}

@test "dies when gh pr edit fails" {
    mock_gh 'pr edit.*--add-reviewer copilot-pull-request-reviewer' 'error' 1
    run bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 1 ]
}