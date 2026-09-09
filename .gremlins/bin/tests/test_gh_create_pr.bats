#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_create_pr"
    ARTIFACT_DIR="$(mktemp -d)"
    BODY_FILE="$ARTIFACT_DIR/pr-body.md"
    NUMBER_FILE="$ARTIFACT_DIR/pr-number.txt"
    URL_FILE="$ARTIFACT_DIR/pr-url.txt"
    printf 'PR body content\n' > "$BODY_FILE"
}

teardown() {
    teardown_mocks
    rm -rf "$ARTIFACT_DIR"
}

@test "creates PR and writes number and URL files" {
    mock_cmd 'python3' '.*random.*choices.*' 'abcd'
    mock_git 'push origin.*HEAD:refs/heads/' '' 0
    mock_gh 'pr create' 'https://github.com/owner/repo/pull/99'
    run bash "$SCRIPT" "main" "My PR Title" "$BODY_FILE" "$NUMBER_FILE" "$URL_FILE"
    [ "$status" -eq 0 ]
    [ "$(cat "$NUMBER_FILE")" = "99" ]
    [ "$(cat "$URL_FILE")" = "https://github.com/owner/repo/pull/99" ]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}

@test "dies when git push fails" {
    mock_cmd 'python3' '.*random.*choices.*' 'abcd'
    mock_git 'push origin.*HEAD:refs/heads/' 'fatal: remote rejected' 1
    run bash "$SCRIPT" "main" "My PR Title" "$BODY_FILE" "$NUMBER_FILE" "$URL_FILE"
    [ "$status" -eq 1 ]
}