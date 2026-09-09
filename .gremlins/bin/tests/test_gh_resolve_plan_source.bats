#!/usr/bin/env bats

load helpers/mocks

setup() {
    setup_mocks
    SCRIPT="$BATS_TEST_DIRNAME/../gh_resolve_plan_source"
    PLAN_FILE="$(mktemp)"
}

teardown() {
    teardown_mocks
    rm -f "$PLAN_FILE"
}

@test "exits 0 when first line does not match issue ref" {
    printf '# Some Plan\n\nContent\n' > "$PLAN_FILE"
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 0 ]
    # Plan file unchanged.
    grep -q '# Some Plan' "$PLAN_FILE"
}

@test "resolves #N and rewrites plan with title/body" {
    printf '#42\n' > "$PLAN_FILE"
    mock_gh 'issue view 42.*--json title,body,number' 'Issue Title	Issue body text	42'
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 0 ]
    [ "$output" = "42" ]
    grep -q '# Issue Title' "$PLAN_FILE"
    grep -q 'Issue body text' "$PLAN_FILE"
}

@test "resolves owner/repo#N with repo arg" {
    printf 'myorg/myrepo#99\n' > "$PLAN_FILE"
    mock_gh 'issue view 99.*--repo myorg/myrepo.*--json title,body,number' 'Repo Issue	Body here	99'
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 0 ]
    [ "$output" = "99" ]
    grep -q '# Repo Issue' "$PLAN_FILE"
}

@test "writes to output file when provided" {
    printf '#42\n' > "$PLAN_FILE"
    OUTPUT_FILE="$(mktemp)"
    mock_gh 'issue view 42.*--json title,body,number' 'T	B	42'
    run bash "$SCRIPT" "$PLAN_FILE" "$OUTPUT_FILE"
    [ "$status" -eq 0 ]
    [ "$(cat "$OUTPUT_FILE")" = "42" ]
    rm -f "$OUTPUT_FILE"
}

@test "preserves body when it starts with #" {
    printf '#42\n' > "$PLAN_FILE"
    mock_gh 'issue view 42.*--json title,body,number' 'T	# existing markdown body	42'
    run bash "$SCRIPT" "$PLAN_FILE"
    [ "$status" -eq 0 ]
    # When body starts with #, it's used directly without adding title.
    grep -q '# existing markdown body' "$PLAN_FILE"
    ! grep -q '# T' "$PLAN_FILE"
}
