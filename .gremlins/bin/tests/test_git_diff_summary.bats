#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../git_diff_summary"
}

teardown() {
    teardown_mocks
}

@test "uses base_ref..HEAD when ancestor" {
    mock_git 'merge-base --is-ancestor main' '' 0
    mock_git 'log --oneline main..HEAD' 'abc1234 fix: thing'
    mock_git 'diff --stat main..HEAD' 'src/main.py | 2 +-'
    run bash "$SCRIPT" "main"
    [ "$status" -eq 0 ]
    [[ "$output" == *"fix: thing"* ]]
    [[ "$output" == *"src/main.py"* ]]
}

@test "falls back to HEAD~10 when not ancestor" {
    mock_git 'merge-base --is-ancestor main' '' 1
    mock_git 'rev-parse --verify HEAD~10' 'def5678' 0
    mock_git 'log --oneline HEAD~10..HEAD' 'abc1234 fix: thing'
    mock_git 'diff --stat HEAD~10..HEAD' 'src/main.py | 2 +-'
    run bash "$SCRIPT" "main"
    [ "$status" -eq 0 ]
    [[ "$output" == *"fix: thing"* ]]
}

@test "falls back to root when even HEAD~10 fails" {
    mock_git 'merge-base --is-ancestor main' '' 1
    mock_git 'rev-parse --verify HEAD~10' '' 1
    mock_git 'rev-list --max-parents=0 HEAD' 'rootSHA'
    mock_git 'log --oneline rootSHA..HEAD' 'abc1234 initial'
    mock_git 'diff --stat rootSHA..HEAD' 'all files'
    run bash "$SCRIPT" "main"
    [ "$status" -eq 0 ]
    [[ "$output" == *"initial"* ]]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}