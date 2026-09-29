#!/usr/bin/env bats
#
# Tests for ci_push_fix — guarded push for CI fix iterations.

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../ci_push_fix"
    BRANCH="feature-branch"
    # Fake — real git is mocked; just needs to be a stable SHA for comparisons.
    LOCAL_SHA="f47ac10b58cc4379a567f0b02c1e3a4567890abc"
}

teardown() {
    teardown_mocks
}

@test "no-op when HEAD is already on remote" {
    mock_git "ls-remote.*${BRANCH}" "$LOCAL_SHA"
    mock_git "rev-parse"        "$LOCAL_SHA"
    # No mock for git push — if it's called the test fails via mock dispatch.

    run bash "$SCRIPT" "$BRANCH"
    [ "$status" -eq 0 ]
    [[ "$output" == *"nothing to push"* ]]
}

@test "pushes when HEAD differs from remote" {
    mock_git "ls-remote.*${BRANCH}" "abc123def456"
    mock_git "rev-parse"           "$LOCAL_SHA"
    mock_git "push.*refs/heads/${BRANCH}" "pushed"

    run bash "$SCRIPT" "$BRANCH"
    [ "$status" -eq 0 ]
    [[ "$output" == *"pushing HEAD"* ]]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}