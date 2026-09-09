#!/usr/bin/env bash
# Shared GitHub CLI helpers.
# Source this relative to the script's own location:
#   SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$SCRIPT_DIR/lib/gh.sh"

# gh_json — call gh with --json and return parsed JSON on stdout.
# Dies on non-zero exit.
gh_json() {
    local output
    output="$(gh "$@")" || die "gh $* failed"
    printf '%s\n' "$output"
}

# gh_api_paginated — call gh api with --paginate, return concatenated JSON.
gh_api_paginated() {
    gh api "$@" --paginate || die "gh api $* --paginate failed"
}

# pr_view_json — gh pr view "$url" --json "$fields"
pr_view_json() {
    local url="$1"
    local fields="$2"
    gh pr view "$url" --json "$fields" || die "gh pr view $url --json $fields failed"
}