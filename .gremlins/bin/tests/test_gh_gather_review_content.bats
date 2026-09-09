#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_gather_review_content"
    PR_URL="https://github.com/owner/repo/pull/42"
}

teardown() {
    teardown_mocks
}

@test "gathers PR metadata and comments into markdown" {
    # gh pr view for metadata (null-delimited) — use fixture file with real null bytes.
    mock_gh_file "pr view $PR_URL.*--json number,title,body,author,headRefName,baseRefName" \
        "$BATS_TEST_DIRNAME/fixtures/gh_gather_pr_meta.bin"
    # gh api for review comments.
    mock_gh "api.*repos/owner/repo/pulls/42/comments.*--paginate" \
        "$(cat "$BATS_TEST_DIRNAME/fixtures/gh_gather_review_comments.txt")"
    # gh pr view for issue comments.
    mock_gh "pr view $PR_URL.*--comments.*--json comments" \
        "$(cat "$BATS_TEST_DIRNAME/fixtures/gh_gather_issue_comments.json")"

    run bash "$SCRIPT" "$PR_URL"
    [ "$status" -eq 0 ]
    [[ "$output" == *"# PR Review Comments"* ]]
    [[ "$output" == *"**PR:** https://github.com/owner/repo/pull/42"* ]]
    [[ "$output" == *"**Number:** #42"* ]]
    [[ "$output" == *"**Title:** PR Title"* ]]
    [[ "$output" == *"**Author:** author1"* ]]
    [[ "$output" == *"**Branch:** feature-branch → main"* ]]
    [[ "$output" == *"**Owner/Repo:** owner/repo"* ]]
    [[ "$output" == *"## PR Description"* ]]
    [[ "$output" == *"PR Body"* ]]
    [[ "$output" == *"## Review Comments"* ]]
    [[ "$output" == *"Please fix this"* ]]
    [[ "$output" == *"## Issue Comments"* ]]
    [[ "$output" == *"Great work!"* ]]
}

@test "skips PR description when body is null" {
    mock_gh_file "pr view $PR_URL.*--json number,title,body,author,headRefName,baseRefName" \
        "$BATS_TEST_DIRNAME/fixtures/gh_gather_pr_meta_null_body.bin"
    mock_gh "api.*repos/owner/repo/pulls/42/comments.*--paginate" ''
    mock_gh "pr view $PR_URL.*--comments.*--json comments" '{"comments":[]}'

    run bash "$SCRIPT" "$PR_URL"
    [ "$status" -eq 0 ]
    [[ "$output" != *"## PR Description"* ]]
}

@test "dies without arguments" {
    run bash "$SCRIPT"
    [ "$status" -eq 1 ]
    [[ "$output" == *"Usage"* ]]
}
