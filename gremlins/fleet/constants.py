"""Fleet manager constants."""

import os

BG_STALL_SECS = int(os.environ.get("BG_STALL_SECS") or 2700)

STATE_ROOT = os.path.join(
    os.environ.get("XDG_STATE_HOME", os.path.join(os.path.expanduser("~"), ".local", "state")),
    "claude-gremlins",
)

FMT = "%-5s  %-47s  %-22s  %-28s  %-5s  %-20s  %-10s  %s"

# Headless rescue caps. The attempt cap is shared across interactive and
# headless rescues — both check `rescue_count`, but interactive only warns
# while headless hard-refuses. The wall-clock timeout bounds the diagnosis
# step so a stuck `claude -p` doesn't hang an unattended caller indefinitely.
RESCUE_CAP = 3
try:
    HEADLESS_DIAGNOSIS_TIMEOUT_SECS = int(
        os.environ.get("HEADLESS_RESCUE_TIMEOUT_SECS") or 1800
    )
except (ValueError, TypeError):
    # A misconfigured env var must not break the rest of /gremlins (listing,
    # stop, rm, close, land). Fall back silently to the default.
    HEADLESS_DIAGNOSIS_TIMEOUT_SECS = 1800

# Bail classes the upstream stages may write into state.json.bail_class.
# The first three are excluded from headless rescue: the spec is explicit
# that interpreting reviewer-blocking changes, security findings, or
# secrets-touching diffs autonomously is not safe. `other` is attempted.
EXCLUDED_BAIL_CLASSES = ("reviewer_requested_changes", "security", "secrets")
