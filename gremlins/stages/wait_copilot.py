"""Copilot review polling stage for the gh pipeline."""

from __future__ import annotations

import dataclasses
import time
from collections.abc import Callable

from ..gh_utils import check_copilot_review
from .context import StageContext
from .registry import register_stage


@dataclasses.dataclass
class WaitCopilotOptions:
    repo: str
    pr_num: str
    timeout: int = 600
    interval: int = 20
    review_checker: Callable[[], str | None] | None = None


def run(_: StageContext, options: WaitCopilotOptions) -> str:
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


register_stage("wait-copilot", run)
