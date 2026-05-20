"""Copilot review polling stage for the gh pipeline."""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Callable
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils.github import check_copilot_review_async

logger = logging.getLogger(__name__)


class GitHubWaitCopilot(Stage):
    type = "github-wait-copilot"
    needs_gh = True

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_num: str = "",
        timeout: int = 600,
        interval: int = 20,
        max_poll_failures: int = 5,
        review_checker: Callable[[], str | None] | None = None,
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.pr_num = pr_num
        self.timeout = timeout
        self.interval = interval
        self.max_poll_failures = max_poll_failures
        self.review_checker = review_checker

    async def run(self, state: State) -> Outcome:
        repo = state.repo
        pr_num = self.pr_num or state.data.read_pr_num()
        if not pr_num:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")

        deadline = time.time() + self.timeout
        consecutive_failures = 0
        last_error: str = ""
        poll_count = 0
        observed_states: list[str] = []

        while True:
            try:
                if self.review_checker is not None:
                    result = self.review_checker()
                else:
                    result = await check_copilot_review_async(repo, pr_num)
                consecutive_failures = 0
                poll_count += 1
                if result:
                    observed_states.append(result)
                    logger.info("Copilot review: %s", result)
                    return Done()
            except Exception as exc:
                consecutive_failures += 1
                last_error = str(exc)
                logger.warning(
                    "Copilot review poll failed (%d/%d): %s",
                    consecutive_failures,
                    self.max_poll_failures,
                    last_error,
                )
                if consecutive_failures >= self.max_poll_failures:
                    raise Bail(
                        f"Copilot review poll failed {consecutive_failures} times in a row: {last_error}"
                    )

            if time.time() >= deadline:
                context = (
                    f"polls={poll_count}, last_error={last_error!r}, "
                    f"observed={observed_states or 'none'}"
                )
                raise Bail(f"Copilot review timed out after {self.timeout}s ({context})")

            await asyncio.sleep(self.interval)
