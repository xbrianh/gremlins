"""Copilot review polling stage and request-copilot helper for the gh pipeline."""

from __future__ import annotations

import subprocess
import time
from collections.abc import Callable


def run_request_copilot_stage(*, repo: str, pr_num: str) -> None:
    """Add copilot-pull-request-reviewer to the PR's reviewer list."""
    r = subprocess.run(
        [
            "gh",
            "pr",
            "edit",
            pr_num,
            "--repo",
            repo,
            "--add-reviewer",
            "copilot-pull-request-reviewer",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"could not request Copilot review (is it enabled in repo settings?): "
            f"{r.stderr.strip()}"
        )


def run_wait_copilot_stage(
    *,
    repo: str,
    pr_num: str,
    timeout: int = 600,
    interval: int = 20,
    review_checker: Callable[[], str | None] | None = None,
) -> str:
    """Poll until Copilot posts a non-PENDING review or timeout expires.

    ``review_checker`` is injectable for tests: a zero-argument callable that
    returns the review state string (e.g. ``'APPROVED'``) or ``None`` when the
    review isn't ready yet.  Defaults to a real ``gh api`` call.

    Returns the final review state string.
    """
    from ..gh_utils import check_copilot_review

    if review_checker is None:

        def _default_checker() -> str | None:
            return check_copilot_review(repo, pr_num)

        review_checker = _default_checker

    deadline = time.time() + timeout
    while True:
        state = review_checker()
        if state:
            return state
        if time.time() >= deadline:
            raise RuntimeError(f"Copilot review timed out after {timeout}s")
        time.sleep(interval)
