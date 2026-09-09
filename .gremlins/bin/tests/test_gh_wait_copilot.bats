#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_wait_copilot"
}

teardown() {
    teardown_mocks
}

@test "outputs APPROVED when copilot review is done" {
    mock_gh 'api.*repos/owner/repo/pulls/42/reviews' 'APPROVED'
    run bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
    [ "$output" = "APPROVED" ]
}

@test "outputs CHANGES_REQUESTED when review requests changes" {
    mock_gh 'api.*repos/owner/repo/pulls/42/reviews' 'CHANGES_REQUESTED'
    run bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
    [ "$output" = "CHANGES_REQUESTED" ]
}

@test "exits 0 with empty output when no review found (one-shot)" {
    mock_gh 'api.*repos/owner/repo/pulls/42/reviews' ''
    run bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
    [ -z "$output" ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}