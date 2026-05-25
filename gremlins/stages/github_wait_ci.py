"""CI gate stage — loop + poll + fix composition."""

from __future__ import annotations

import asyncio
import logging
import time
from collections.abc import Callable
from typing import Any

from gremlins.artifacts.registry import MissingArtifact
from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils.git import head_sha
from gremlins.utils.github import fetch_check_run_logs_async, get_pr_ci_status_async

logger = logging.getLogger(__name__)

_FAILING_CONCLUSIONS = frozenset({"FAILURE", "ERROR", "TIMED_OUT", "CANCELLED"})
_PENDING_STATES = frozenset({"EXPECTED", "PENDING"})


def _is_done(check: dict[str, Any]) -> bool:
    if check.get("__typename") == "StatusContext":
        return check.get("state") not in _PENDING_STATES
    return check.get("status") == "COMPLETED"


def _is_failing(check: dict[str, Any]) -> bool:
    if check.get("__typename") == "StatusContext":
        return check.get("state") in ("FAILURE", "ERROR")
    return check.get("conclusion") in _FAILING_CONCLUSIONS


async def _fetch_checks(
    pr_url: str,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
) -> tuple[list[dict[str, Any]], str]:
    if checks_getter is not None:
        return checks_getter()
    status = await get_pr_ci_status_async(pr_url)
    return status["checks"], status["review_decision"]


async def _fetch_current_status(
    pr_url: str,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
    head_sha_getter: Callable[[], str] | None,
) -> tuple[list[dict[str, Any]], str, str]:
    if checks_getter is not None:
        checks, review_decision = checks_getter()
        current_sha = head_sha_getter() if head_sha_getter is not None else ""
        return checks, review_decision, current_sha
    status = await get_pr_ci_status_async(pr_url)
    return status["checks"], status["review_decision"], status["head_sha"]


async def _wait_for_checks(
    pr_url: str,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
    poll_interval: int,
    grace_secs: int,
) -> tuple[list[dict[str, Any]], str]:
    deadline = time.time() + grace_secs
    review_decision = ""
    while True:
        checks, review_decision = await _fetch_checks(pr_url, checks_getter)
        if checks or review_decision == "REVIEW_REQUIRED" or time.time() >= deadline:
            return checks, review_decision
        await asyncio.sleep(poll_interval)


def _bail_if_review_required(state: State, decision: str) -> None:
    if decision != "REVIEW_REQUIRED":
        return
    state.record_bail("PR requires human review approval before merge")
    raise Bail("ci-gate: PR blocked by required human review")


async def _poll_until_done(
    state: State,
    pr_url: str,
    timeout: int,
    interval: int,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None,
    required_sha: str = "",
    head_sha_getter: Callable[[], str] | None = None,
) -> tuple[list[dict[str, Any]], str]:
    deadline = time.time() + timeout
    while True:
        checks, review_decision, current_sha = await _fetch_current_status(
            pr_url, checks_getter, head_sha_getter
        )
        _bail_if_review_required(state, review_decision)

        if required_sha and current_sha and current_sha != required_sha:
            if time.time() >= deadline:
                raise Bail(
                    f"ci-gate: timed out waiting for GitHub to reflect pushed SHA "
                    f"{required_sha[:8]} (still showing {current_sha[:8]}) after {timeout}s"
                )
            logger.debug(
                "ci-gate: PR head %s != expected %s, waiting for push to propagate",
                current_sha[:8],
                required_sha[:8],
            )
            await asyncio.sleep(interval)
            continue

        if checks and all(_is_done(c) for c in checks):
            return checks, review_decision
        if time.time() >= deadline:
            logger.info("ci-gate: poll timed out after %ds", timeout)
            return checks, review_decision
        logger.debug("ci-gate: checks still pending, sleeping %ds", interval)
        await asyncio.sleep(interval)


async def _collect_failure_output(failed: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for check in failed:
        name = check.get("name") or check.get("context") or "unknown"
        details = check.get("detailsUrl") or check.get("targetUrl") or ""
        logs = await fetch_check_run_logs_async(details)
        header = f"## Check: {name}"
        parts.append(
            f"{header}\n\n{logs}" if logs else f"{header}\n\n(no output available)"
        )
    return "\n\n".join(parts)


class _CIPollStage(Stage):
    """Polls CI for one attempt; returns Done on pass, NeedsFix on failure."""

    type = "_ci_poll"

    def __init__(
        self,
        *,
        pr_url: str,
        poll_timeout: int,
        poll_interval: int,
        startup_grace_secs: int,
        checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
        head_sha_getter: Callable[[], str] | None,
        attempt_no: list[int],
        fix_sha: list[str],
    ) -> None:
        super().__init__("poll")
        self._pr_url = pr_url
        self._poll_timeout = poll_timeout
        self._poll_interval = poll_interval
        self._startup_grace_secs = startup_grace_secs
        self._checks_getter = checks_getter
        self._head_sha_getter = head_sha_getter
        self._attempt_no = attempt_no
        self._fix_sha = fix_sha
        self._first = True

    async def run(self, state: State) -> Outcome:
        pr_url = self._pr_url or state.artifacts.read("pr").url
        required_sha = ""

        if self._first:
            self._first = False
            checks, review_decision = await _wait_for_checks(
                pr_url, self._checks_getter, self._poll_interval, self._startup_grace_secs
            )
            _bail_if_review_required(state, review_decision)
            if not checks:
                logger.info(
                    "ci-gate: no check-runs after %ds grace, skipping",
                    self._startup_grace_secs,
                )
                return Done()
        else:
            required_sha = self._fix_sha[0]

        self._attempt_no[0] += 1
        n = self._attempt_no[0]
        logger.info(
            "ci-gate: attempt %d — polling (timeout %ds)", n, self._poll_timeout
        )
        final_checks, _ = await _poll_until_done(
            state,
            pr_url,
            self._poll_timeout,
            self._poll_interval,
            self._checks_getter,
            required_sha=required_sha,
            head_sha_getter=self._head_sha_getter,
        )
        failed = [c for c in final_checks if _is_failing(c)]
        if not failed:
            logger.info("ci-gate: all checks passed on attempt %d", n)
            return Done()

        logger.info("ci-gate: %d check(s) failed on attempt %d", len(failed), n)
        failure_output = await _collect_failure_output(failed)
        (state.session_dir / f"ci-attempt-{n}.log").write_text(
            failure_output, encoding="utf-8"
        )
        return NeedsFix(failure_output, 1)


class _CIFixStage(Stage):
    """Runs the CI-fix agent for the current attempt."""

    type = "_ci_fix"

    def __init__(
        self,
        *,
        template: str,
        attempt_no: list[int],
        fix_sha: list[str],
        fix_sha_getter: Callable[[], str] | None,
    ) -> None:
        super().__init__("fix")
        self._template = template
        self._attempt_no = attempt_no
        self._fix_sha = fix_sha
        self._fix_sha_getter = fix_sha_getter

    async def run(self, state: State) -> Outcome:
        n = self._attempt_no[0]
        log_file = state.session_dir / f"ci-attempt-{n}.log"
        failure_output = log_file.read_text(encoding="utf-8") if log_file.exists() else ""

        try:
            pr_branch = state.artifacts.read("pr").branch
        except MissingArtifact:
            state.record_bail("ci-fix: pr not in registry, cannot push")
            raise Bail("ci-fix: pr not in registry, cannot push")

        if not pr_branch:
            state.record_bail("ci-fix: pr branch is empty, cannot push")
            raise Bail("ci-fix: pr branch is empty, cannot push")

        fix_prompt = self._template.format(
            failure_output=failure_output, pr_branch=pr_branch
        )
        await run_agent(
            state,
            fix_prompt,
            label=f"ci-fix-{n}",
            raw_path=state.session_dir / f"stream-ci-fix-{n}.jsonl",
        )
        self._fix_sha[0] = (
            self._fix_sha_getter() if self._fix_sha_getter is not None
            else head_sha(cwd=state.cwd)
        )
        return Done()


class GitHubWaitCI(Stage):
    type = "github-wait-ci"
    needs_gh = True

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_url: str = "",
        max_attempts: int = 3,
        poll_timeout: int = 1200,
        poll_interval: int = 30,
        startup_grace_secs: int = 60,
        checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None,
        head_sha_getter: Callable[[], str] | None = None,
        fix_sha_getter: Callable[[], str] | None = None,
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.pr_url = pr_url
        self.max_attempts = max_attempts
        self.poll_timeout = poll_timeout
        self.poll_interval = poll_interval
        self.startup_grace_secs = startup_grace_secs
        self.checks_getter = checks_getter
        self.head_sha_getter = head_sha_getter
        self.fix_sha_getter = fix_sha_getter

    async def run(self, state: State) -> Outcome:
        template = "\n\n".join(self.prompts).rstrip()
        attempt_no: list[int] = [0]
        fix_sha: list[str] = [""]
        poll = _CIPollStage(
            pr_url=self.pr_url,
            poll_timeout=self.poll_timeout,
            poll_interval=self.poll_interval,
            startup_grace_secs=self.startup_grace_secs,
            checks_getter=self.checks_getter,
            head_sha_getter=self.head_sha_getter,
            attempt_no=attempt_no,
            fix_sha=fix_sha,
        )
        fix = _CIFixStage(
            template=template,
            attempt_no=attempt_no,
            fix_sha=fix_sha,
            fix_sha_getter=self.fix_sha_getter,
        )
        return await LoopStage(
            self.name, body=[poll, fix], max_iterations=self.max_attempts
        ).run(state)
