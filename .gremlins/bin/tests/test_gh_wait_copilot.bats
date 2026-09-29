#!/usr/bin/env bats
bats_require_minimum_version 1.5.0

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_wait_copilot"
}

teardown() {
    teardown_mocks
}

@test "outputs APPROVED when copilot review is done" {
    mock_gh 'api.*pulls/42.*head\.sha' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    mock_gh 'api.*check-runs' '{"status":"completed","conclusion":"neutral"}'
    mock_gh 'api.*pulls/42/reviews' 'APPROVED'
    run --separate-stderr bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
    [ "$output" = "APPROVED" ]
    [[ "$stderr" == *"checking for Copilot review"* ]]
    [[ "$stderr" == *"review found"* ]]
}

@test "outputs CHANGES_REQUESTED when review requests changes" {
    mock_gh 'api.*pulls/42.*head\.sha' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    mock_gh 'api.*check-runs' '{"status":"completed","conclusion":"neutral"}'
    mock_gh 'api.*pulls/42/reviews' 'CHANGES_REQUESTED'
    run --separate-stderr bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
    [ "$output" = "CHANGES_REQUESTED" ]
    [[ "$stderr" == *"checking for Copilot review"* ]]
    [[ "$stderr" == *"review found"* ]]
}

@test "exits 0 with empty output when no review found (one-shot)" {
    mock_gh 'api.*pulls/42.*head\.sha' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
    mock_gh 'api.*check-runs' '{"status":"in_progress","conclusion":null}'
    run --separate-stderr bash "$SCRIPT" 42 "owner/repo"
    [ "$status" -eq 0 ]
    [ -z "$output" ]
    [[ "$stderr" == *"still waiting"* ]]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}