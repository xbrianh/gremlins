"""Import all stage modules so each self-registers into the stage registry."""

from gremlins.stages import (
    address_code,
    commit,
    implement,
    open_github_pr,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)

__all__ = [
    "address_code",
    "commit",
    "implement",
    "open_github_pr",
    "plan",
    "request_copilot",
    "review_code",
    "verify",
    "wait_ci",
    "wait_copilot",
]
