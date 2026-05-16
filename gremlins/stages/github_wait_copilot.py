"""Copilot review polling stage for the gh pipeline."""

from __future__ import annotations

import logging
import time
from collections.abc import Callable
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils.github import check_copilot_review

logger = logging.getLogger(__name__)


class GitHubWaitCopilot(Stage):
    type = "github-wait-copilot"
    needs_gh = True

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> GitHubWaitCopilot:  # noqa: ARG002
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_num: str = "",
        timeout: int = 600,
        interval: int = 20,
        review_checker: Callable[[], str | None] | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.pr_num = pr_num
        self.timeout = timeout
        self.interval = interval
        self.review_checker = review_checker

    def run(self, state: State) -> Outcome:
        repo = state.repo
        pr_num = self.pr_num or state.data.read_pr_num()
        if not pr_num:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")

        review_checker = self.review_checker
        if review_checker is None:

            def _default_checker() -> str | None:
                return check_copilot_review(repo, pr_num)

            review_checker = _default_checker

        deadline = time.time() + self.timeout
        while True:
            result = review_checker()
            if result:
                logger.info("Copilot review: %s", result)
                return Done()
            if time.time() >= deadline:
                return Bail(f"Copilot review timed out after {self.timeout}s")
            time.sleep(self.interval)
