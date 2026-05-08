"""Copilot review polling stage for the gh pipeline."""

from __future__ import annotations

import time
from collections.abc import Callable
from typing import Any

from gremlins.gh_utils import check_copilot_review
from gremlins.pipeline import StageEntry
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import read_pr_num


class WaitCopilot(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        repo: str,
        pr_num: str = "",
        timeout: int = 600,
        interval: int = 20,
        review_checker: Callable[[], str | None] | None = None,
    ) -> None:
        super().__init__(entry, model)
        self.repo = repo
        self.pr_num = pr_num
        self.timeout = timeout
        self.interval = interval
        self.review_checker = review_checker

    def run(self, pipe: Any) -> str:
        pr_num = self.pr_num or read_pr_num(self.state.gr_id)
        if not pr_num:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")

        review_checker = self.review_checker
        if review_checker is None:

            def _default_checker() -> str | None:
                return check_copilot_review(self.repo, pr_num)

            review_checker = _default_checker

        deadline = time.time() + self.timeout
        while True:
            state = review_checker()
            if state:
                return state
            if time.time() >= deadline:
                raise RuntimeError(f"Copilot review timed out after {self.timeout}s")
            time.sleep(self.interval)


register_stage("wait-copilot", WaitCopilot)
