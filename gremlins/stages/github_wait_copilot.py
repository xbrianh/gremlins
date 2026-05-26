"""Copilot review polling stage — loop + poll composition."""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Callable
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils.github import check_copilot_review_async

logger = logging.getLogger(__name__)


class _CopilotPollStage(Stage):
    """Single poll of the copilot review API; returns Done when review is posted."""

    type = "_copilot_poll"

    def __init__(
        self,
        *,
        pr_num: str,
        timeout: int,
        interval: int,
        max_poll_failures: int,
        review_checker: Callable[[], str | None] | None,
    ) -> None:
        super().__init__("poll")
        self._pr_num = pr_num
        self._timeout = timeout
        self._interval = interval
        self._max_poll_failures = max_poll_failures
        self._review_checker = review_checker
        self._deadline: float | None = None
        self._consecutive_failures = 0
        self._poll_count = 0
        self._last_error = ""

    async def run(self, state: State) -> Outcome:
        if self._deadline is None:
            self._deadline = time.time() + self._timeout

        if self._poll_count > 0:
            await asyncio.sleep(self._interval)
        self._poll_count += 1

        repo = state.engine_ctx.repo
        pr_num = self._pr_num or str(state.artifacts.read("pr").number)

        try:
            if self._review_checker is not None:
                result = self._review_checker()
            else:
                result = await check_copilot_review_async(repo, pr_num)
            self._consecutive_failures = 0
            if result:
                logger.info("Copilot review: %s", result)
                return Done()
        except Exception as exc:
            self._consecutive_failures += 1
            self._last_error = str(exc)
            logger.warning(
                "Copilot review poll failed (%d/%d): %s",
                self._consecutive_failures,
                self._max_poll_failures,
                self._last_error,
            )
            if self._consecutive_failures >= self._max_poll_failures:
                raise Bail(
                    f"Copilot review poll failed {self._consecutive_failures} times"
                    f" in a row: {self._last_error}"
                )

        if time.time() >= self._deadline:
            context = f"polls={self._poll_count}, last_error={self._last_error!r}"
            raise Bail(f"Copilot review timed out after {self._timeout}s ({context})")

        return NeedsFix("waiting for copilot review")


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
        poll = _CopilotPollStage(
            pr_num=self.pr_num,
            timeout=self.timeout,
            interval=self.interval,
            max_poll_failures=self.max_poll_failures,
            review_checker=self.review_checker,
        )
        max_iterations = (
            self.timeout // max(1, self.interval) + self.max_poll_failures + 2
        )
        return await LoopStage(
            self.name, body=[poll], max_iterations=max(max_iterations, 10)
        ).run(state)
