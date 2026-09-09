# bats-compatible mocking helpers for gh and git.
#
# Usage in a bats test:
#   load helpers/mocks
#   setup() { setup_mocks; }
#   teardown() { teardown_mocks; }
#   @test "something" {
#       mock_gh 'repo view.*nameWithOwner' '{"nameWithOwner":"owner/repo"}'
#       run gh_discover_repo
#       [ "$status" -eq 0 ]
#       [ "$output" = "owner/repo" ]
#   }
#
# Each mock is consumed on first match, so multiple calls with the
# same pattern return different data in registration order.

MOCK_DIR=""
MOCK_NEXT_ID=0

mock_gh() {
    local pattern="$1"
    local stdout_content="${2:-}"
    local exit_code="${3:-0}"
    MOCK_NEXT_ID=$((MOCK_NEXT_ID + 1))
    local stdout_file="$MOCK_DIR/gh_stdout_${MOCK_NEXT_ID}"
    local exit_code_file="$MOCK_DIR/gh_exit_${MOCK_NEXT_ID}"
    printf '%s\n' "$stdout_content" > "$stdout_file"
    printf '%d\n' "$exit_code" > "$exit_code_file"
    printf '%s|%s|%s\n' "$pattern" "$stdout_file" "$exit_code_file" >> "$MOCK_DIR/gh_patterns"
}

mock_gh_file() {
    local pattern="$1"
    local src_file="$2"
    local exit_code="${3:-0}"
    MOCK_NEXT_ID=$((MOCK_NEXT_ID + 1))
    local stdout_file="$MOCK_DIR/gh_stdout_${MOCK_NEXT_ID}"
    local exit_code_file="$MOCK_DIR/gh_exit_${MOCK_NEXT_ID}"
    cp "$src_file" "$stdout_file"
    printf '%d\n' "$exit_code" > "$exit_code_file"
    printf '%s|%s|%s\n' "$pattern" "$stdout_file" "$exit_code_file" >> "$MOCK_DIR/gh_patterns"
}

mock_git() {
    local pattern="$1"
    local stdout_content="${2:-}"
    local exit_code="${3:-0}"
    MOCK_NEXT_ID=$((MOCK_NEXT_ID + 1))
    local stdout_file="$MOCK_DIR/git_stdout_${MOCK_NEXT_ID}"
    local exit_code_file="$MOCK_DIR/git_exit_${MOCK_NEXT_ID}"
    printf '%s\n' "$stdout_content" > "$stdout_file"
    printf '%d\n' "$exit_code" > "$exit_code_file"
    printf '%s|%s|%s\n' "$pattern" "$stdout_file" "$exit_code_file" >> "$MOCK_DIR/git_patterns"
}

mock_git_file() {
    local pattern="$1"
    local src_file="$2"
    local exit_code="${3:-0}"
    MOCK_NEXT_ID=$((MOCK_NEXT_ID + 1))
    local stdout_file="$MOCK_DIR/git_stdout_${MOCK_NEXT_ID}"
    local exit_code_file="$MOCK_DIR/git_exit_${MOCK_NEXT_ID}"
    cp "$src_file" "$stdout_file"
    printf '%d\n' "$exit_code" > "$exit_code_file"
    printf '%s|%s|%s\n' "$pattern" "$stdout_file" "$exit_code_file" >> "$MOCK_DIR/git_patterns"
}

# mock_cmd — mock an arbitrary command (e.g., python3, jq) for one-shot use.
mock_cmd() {
    local cmd_name="$1"
    local pattern="$2"
    local stdout_content="${3:-}"
    local exit_code="${4:-0}"
    local patterns_file="$MOCK_DIR/cmd_${cmd_name}_patterns"
    if [ ! -f "$patterns_file" ]; then
        : > "$patterns_file"
        cat > "$MOCK_DIR/$cmd_name" << 'CMDMOCK'
#!/usr/bin/env bash
CMD_NAME="$(basename "$0")"
pf="$(dirname "$0")/cmd_${CMD_NAME}_patterns"
if [ ! -f "$pf" ]; then
    echo "MOCK FAIL: no patterns for $CMD_NAME" >&2
    exit 99
fi
tmpfile="$(mktemp "${pf}.tmp.XXXXXX")"
matched=0
matched_exit=0
while IFS= read -r line; do
    if [ "$matched" -eq 0 ]; then
        pattern="${line%%|*}"
        rest="${line#*|}"
        stdout_file="${rest%%|*}"
        exit_code_file="${rest##*|}"
        if echo "$*" | grep -qE "$pattern"; then
            cat "$stdout_file"
            matched_exit="$(cat "$exit_code_file")"
            matched=1
            continue
        fi
    fi
    printf '%s\n' "$line" >> "$tmpfile"
done < "$pf"
mv "$tmpfile" "$pf"
if [ "$matched" -eq 0 ]; then
    echo "MOCK FAIL: unmatched $CMD_NAME call: $*" >&2
    exit 99
fi
exit "$matched_exit"
CMDMOCK
        chmod +x "$MOCK_DIR/$cmd_name"
    fi
    MOCK_NEXT_ID=$((MOCK_NEXT_ID + 1))
    local stdout_file="$MOCK_DIR/cmd_stdout_${MOCK_NEXT_ID}"
    local exit_code_file="$MOCK_DIR/cmd_exit_${MOCK_NEXT_ID}"
    printf '%s\n' "$stdout_content" > "$stdout_file"
    printf '%d\n' "$exit_code" > "$exit_code_file"
    printf '%s|%s|%s\n' "$pattern" "$stdout_file" "$exit_code_file" >> "$patterns_file"
}

setup_mocks() {
    MOCK_DIR="$(mktemp -d /tmp/gremlins-bats-mocks.XXXXXX)"
    MOCK_NEXT_ID=0
    : > "$MOCK_DIR/gh_patterns"
    : > "$MOCK_DIR/git_patterns"

    # Consumer dispatch: first pattern-matching line is removed from the
    # file and its exit code is used.  This lets repeated calls with the
    # same pattern consume successive mocks.
    cat > "$MOCK_DIR/_mock_dispatch" << 'DISPATCH'
#!/usr/bin/env bash
pf="$1"
shift
tmpfile="$(mktemp "${pf}.tmp.XXXXXX")"
matched=0
matched_exit=0
while IFS= read -r line; do
    if [ "$matched" -eq 0 ]; then
        pattern="${line%%|*}"
        rest="${line#*|}"
        stdout_file="${rest%%|*}"
        exit_code_file="${rest##*|}"
        if echo "$*" | grep -qE "$pattern"; then
            cat "$stdout_file"
            matched_exit="$(cat "$exit_code_file")"
            matched=1
            continue
        fi
    fi
    printf '%s\n' "$line" >> "$tmpfile"
done < "$pf"
mv "$tmpfile" "$pf"
if [ "$matched" -eq 0 ]; then
    echo "MOCK FAIL: unmatched call: $*" >&2
    echo "Remaining patterns:" >&2
    cat "$pf" >&2
    exit 99
fi
exit "$matched_exit"
DISPATCH
    chmod +x "$MOCK_DIR/_mock_dispatch"

    cat > "$MOCK_DIR/gh" << 'GHMOCK'
#!/usr/bin/env bash
exec "$(dirname "$0")/_mock_dispatch" "$(dirname "$0")/gh_patterns" "$@"
GHMOCK
    chmod +x "$MOCK_DIR/gh"

    cat > "$MOCK_DIR/git" << 'GITMOCK'
#!/usr/bin/env bash
exec "$(dirname "$0")/_mock_dispatch" "$(dirname "$0")/git_patterns" "$@"
GITMOCK
    chmod +x "$MOCK_DIR/git"

    export OLD_PATH="$PATH"
    export PATH="$MOCK_DIR:$PATH"
}

teardown_mocks() {
    export PATH="${OLD_PATH:-$PATH}"
    if [ -n "$MOCK_DIR" ] && [ -d "$MOCK_DIR" ]; then
        rm -rf "$MOCK_DIR"
    fi
}