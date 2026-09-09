#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_push_to_pr_branch"
}

teardown() {
    teardown_mocks
}

@test "pushes HEAD to branch" {
    mock_git 'push origin.*HEAD:refs/heads/my-branch' '' 0
    run bash "$SCRIPT" "my-branch"
    [ "$status" -eq 0 ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}

@test "dies when git push fails" {
    mock_git 'push origin.*HEAD:refs/heads/my-branch' 'rejected' 1
    run bash "$SCRIPT" "my-branch"
    [ "$status" -eq 1 ]
}
