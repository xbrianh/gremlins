#!/usr/bin/env bats
bats_require_minimum_version 1.5.0

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_discover_repo"
}

teardown() {
    teardown_mocks
}

@test "outputs owner/repo from gh repo view" {
    mock_gh 'repo view.*nameWithOwner' 'owner/repo'
    run --separate-stderr bash "$SCRIPT"
    [ "$status" -eq 0 ]
    [ "$output" = "owner/repo" ]
    [[ "$stderr" == *"resolving repo name"* ]]
}

@test "dies when gh fails" {
    mock_gh 'repo view.*nameWithOwner' 'error message' 1
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
}
