"""CI gate stage for the gh pipeline."""

from __future__ import annotations

import logging
import time
from collections.abc import Callable
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.utils.git import head_sha
from gremlins.utils.github import fetch_check_run_logs, get_pr_ci_status

logger = logging.getLogger(__name__)

_FAILING_CONCLUSIONS = frozenset({"FAILURE", "ERROR", "TIMED_OUT", "CANCELLED"})
_PENDING_STATES = frozenset({"EXPECTED", "PENDING"})


class _ReviewRequiredError(RuntimeError):
    pass


def _is_done(check: dict[str, Any]) -> bool:
    if check.get("__typename") == "StatusContext":
        return check.get("state") not in _PENDING_STATES
    return check.get("status") == "COMPLETED"


def _is_failing(check: dict[str, Any]) -> bool:
    if check.get("__typename") == "StatusContext":
        return check.get("state") in ("FAILURE", "ERROR")
    return check.get("conclusion") in _FAILING_CONCLUSIONS


def _fetch_checks(
    pr_url: str,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
) -> tuple[list[dict[str, Any]], str]:
    if checks_getter is not None:
        return checks_getter()
    status = get_pr_ci_status(pr_url)
    return status["checks"], status["review_decision"]


def _wait_for_checks(
    pr_url: str,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
    poll_interval: int,
    grace_secs: int,
) -> tuple[list[dict[str, Any]], str]:
    deadline = time.time() + grace_secs
    review_decision = ""
    while True:
        checks, review_decision = _fetch_checks(pr_url, checks_getter)
        if checks or review_decision == "REVIEW_REQUIRED" or time.time() >= deadline:
            return checks, review_decision
        time.sleep(poll_interval)


def _bail_if_review_required(state: State, decision: str) -> None:
    if decision == "REVIEW_REQUIRED":
        state.write_bail_file("other", "PR requires human review approval before merge")
        raise _ReviewRequiredError("ci-gate: PR blocked by required human review")


def _poll_until_done(
    state: State,
    pr_url: str,
    timeout: int,
    interval: int,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None,
    required_sha: str = "",
    head_sha_getter: Callable[[], str] | None = None,
) -> tuple[list[dict[str, Any]], str]:
    deadline = time.time() + timeout
    review_decision = ""
    while True:
        current_sha = ""
        if checks_getter is not None:
            checks, review_decision = checks_getter()
            if head_sha_getter is not None:
                current_sha = head_sha_getter()
        else:
            status = get_pr_ci_status(pr_url)
            checks = status["checks"]
            review_decision = status["review_decision"]
            current_sha = status["head_sha"]

        _bail_if_review_required(state, review_decision)

        if required_sha and current_sha and current_sha != required_sha:
            if time.time() >= deadline:
                raise RuntimeError(
                    f"ci-gate: timed out waiting for GitHub to reflect pushed SHA "
                    f"{required_sha[:8]} (still showing {current_sha[:8]}) after {timeout}s"
                )
            logger.debug(
                "ci-gate: PR head %s != expected %s, waiting for push to propagate",
                current_sha[:8],
                required_sha[:8],
            )
            time.sleep(interval)
            continue

        if checks and all(_is_done(c) for c in checks):
            return checks, review_decision
        if time.time() >= deadline:
            logger.info("ci-gate: poll timed out after %ds", timeout)
            return checks, review_decision
        logger.debug("ci-gate: checks still pending, sleeping %ds", interval)
        time.sleep(interval)


def _collect_failure_output(failed: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for check in failed:
        name = check.get("name") or check.get("context") or "unknown"
        details = check.get("detailsUrl") or check.get("targetUrl") or ""
        logs = fetch_check_run_logs(details)
        header = f"## Check: {name}"
        parts.append(
            f"{header}\n\n{logs}" if logs else f"{header}\n\n(no output available)"
        )
    return "\n\n".join(parts)


class GitHubWaitCI(Stage):
    type = "github-wait-ci"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> GitHubWaitCI:
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
        pr_url: str = "",
        max_attempts: int = 3,
        poll_timeout: int = 1200,
        poll_interval: int = 30,
        startup_grace_secs: int = 60,
        checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None,
        head_sha_getter: Callable[[], str] | None = None,
        fix_sha_getter: Callable[[], str] | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.pr_url = pr_url
        self.max_attempts = max_attempts
        self.poll_timeout = poll_timeout
        self.poll_interval = poll_interval
        self.startup_grace_secs = startup_grace_secs
        self.checks_getter = checks_getter
        self.head_sha_getter = head_sha_getter
        self.fix_sha_getter = fix_sha_getter

    def run(self, state: State) -> None:
        pr_url = self.pr_url or state.read_pr_url()
        if not pr_url:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        checks, review_decision = _wait_for_checks(
            pr_url, self.checks_getter, self.poll_interval, self.startup_grace_secs
        )
        _bail_if_review_required(state, review_decision)

        if not checks:
            logger.info(
                "ci-gate: PR has no check-runs after %ds, skipping",
                self.startup_grace_secs,
            )
            return

        template = "\n\n".join(self.prompts).rstrip()

        _exhausted = False
        _agent_bailed = False
        _review_bailed = False
        fix_sha = ""
        try:
            for attempt in range(1, self.max_attempts + 1):
                logger.info(
                    "ci-gate: attempt %d/%d — polling (timeout %ds)",
                    attempt,
                    self.max_attempts,
                    self.poll_timeout,
                )
                try:
                    final_checks, review_decision = _poll_until_done(
                        state,
                        pr_url,
                        self.poll_timeout,
                        self.poll_interval,
                        self.checks_getter,
                        required_sha=fix_sha,
                        head_sha_getter=self.head_sha_getter,
                    )
                except _ReviewRequiredError:
                    _review_bailed = True
                    raise
                failed = [c for c in final_checks if _is_failing(c)]

                if not failed:
                    logger.info("ci-gate: all checks passed on attempt %d", attempt)
                    return

                logger.info(
                    "ci-gate: %d check(s) failed on attempt %d", len(failed), attempt
                )

                if attempt == self.max_attempts:
                    break

                failure_output = _collect_failure_output(failed)
                log_file = state.session_dir / f"ci-attempt-{attempt}.log"
                log_file.write_text(failure_output, encoding="utf-8")

                pr_branch = state.last_pr_branch()
                if not pr_branch:
                    state.write_bail_file(
                        "other", "ci-fix: pr_branch unknown, cannot push"
                    )
                    return

                fix_prompt = template.format(
                    bail_command=self.bail_command(state),
                    failure_output=failure_output,
                    pr_branch=pr_branch,
                )
                self.run_claude(
                    fix_prompt,
                    state=state,
                    label=f"ci-fix-{attempt}",
                    raw_path=state.session_dir / f"stream-ci-fix-{attempt}.jsonl",
                )
                _agent_bailed = True
                state.check_bail(f"ci-fix-{attempt}")
                _agent_bailed = False

                fix_sha = (
                    self.fix_sha_getter()
                    if self.fix_sha_getter is not None
                    else head_sha(cwd=state.cwd)
                )

            _exhausted = True
            state.write_bail_file(
                "other",
                f"CI failed after {self.max_attempts} attempts",
            )
            raise RuntimeError(f"ci-gate exhausted {self.max_attempts} attempts")
        except (SystemExit, Exception) as exc:
            if not _exhausted and not _agent_bailed and not _review_bailed:
                state.write_bail_file(
                    "other",
                    f"ci-gate failed: {exc}"[:200],
                )
            raise
