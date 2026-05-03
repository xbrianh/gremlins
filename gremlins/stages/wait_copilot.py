"""Copilot review polling stage and request-copilot helper for the gh pipeline."""

from __future__ import annotations

import dataclasses
import subprocess
import time
from collections.abc import Callable

from ..gh_utils import check_copilot_review
from .context import StageContext


@dataclasses.dataclass
class RequestCopilotOptions:
    repo: str
    pr_num: str


@dataclasses.dataclass
class WaitCopilotOptions:
    repo: str
    pr_num: str
    timeout: int = 600
    interval: int = 20
    review_checker: Callable[[], str | None] | None = None


def run_request_copilot_stage(
    _ctx: StageContext, options: RequestCopilotOptions
) -> None:
    """Add copilot-pull-request-reviewer to the PR's reviewer list."""
    r = subprocess.run(
        [
            "gh",
            "pr",
            "edit",
            options.pr_num,
            "--repo",
            options.repo,
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


def run(_ctx: StageContext, options: WaitCopilotOptions) -> str:
    """Poll until Copilot posts a non-PENDING review or timeout expires."""
    review_checker = options.review_checker
    if review_checker is None:

        def _default_checker() -> str | None:
            return check_copilot_review(options.repo, options.pr_num)

        review_checker = _default_checker

    deadline = time.time() + options.timeout
    while True:
        state = review_checker()
        if state:
            return state
        if time.time() >= deadline:
            raise RuntimeError(f"Copilot review timed out after {options.timeout}s")
        time.sleep(options.interval)
