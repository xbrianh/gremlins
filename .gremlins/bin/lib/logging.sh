#!/usr/bin/env bash
# Shared logging helpers.
# Source this relative to the script's own location:
#   SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
#   source "$SCRIPT_DIR/lib/logging.sh"

die() {
    printf '%s\n' "ERROR: $*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*" >&2
}

warn() {
    printf 'WARNING: %s\n' "$*" >&2
}

bail() {
    printf '%s\n' "$*" >&2
    exit 2
}
