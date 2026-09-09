#!/usr/bin/env bash
# Shared polling helper.
# Source this relative to the script's own location:
#   SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$SCRIPT_DIR/lib/polling.sh"

# poll_until "description" timeout interval check_fn [args...]
# Calls check_fn repeatedly with optional args.
# check_fn returns 0 → success (stdout passed through).
# Returns 1 on timeout.
poll_until() {
    local description="$1"
    local timeout="$2"
    local interval="$3"
    local check_fn="$4"
    shift 4

    local deadline
    deadline=$(( $(date +%s) + timeout ))

    while true; do
        local output
        if output="$("$check_fn" "$@" 2>/dev/null)"; then
            printf '%s\n' "$output"
            return 0
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            printf 'poll_until: %s timed out after %ss\n' "$description" "$timeout" >&2
            return 1
        fi
        sleep "$interval"
    done
}
