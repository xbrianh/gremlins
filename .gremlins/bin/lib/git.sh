#!/usr/bin/env bash
# Shared git helpers.
# Source this relative to the script's own location:
#   SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$SCRIPT_DIR/lib/git.sh"

# push_branch — push HEAD to a remote branch.
push_branch() {
    local remote="${1:-origin}"
    local branch="$2"
    git push "$remote" "HEAD:refs/heads/$branch" || die "git push $remote HEAD:refs/heads/$branch failed"
}

# merge_base_ancestor — check if a commit is an ancestor of HEAD.
# Returns 0 if ancestor, 1 if not.
merge_base_ancestor() {
    local commit="$1"
    git merge-base --is-ancestor "$commit" HEAD 2>/dev/null
}

# rev_parse — get the SHA of HEAD.
rev_parse() {
    git rev-parse HEAD
}