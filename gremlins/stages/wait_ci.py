"""CI gate stage for the gh pipeline.

Polls the PR's CI checks after the review/address stages. On failure, invokes
claude to fix the code and retries. Bounded by max_attempts (default 3) and a
per-attempt poll_timeout (default 20 minutes).
"""

from __future__ import annotations

import logging
import os
import pathlib
import time
from collections.abc import Callable
from typing import Any

from ..clients.claude import ClaudeClient
from ..gh_utils import fetch_check_run_logs, get_pr_ci_status
from ..state import check_bail, emit_bail

logger = logging.getLogger(__name__)

PROMPT_TEMPLATE_PATH = (
    pathlib.Path(__file__).resolve().parent.parent / "prompts" / "ci_fix.md"
)

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


def _bail_if_review_required(decision: str) -> None:
    if decision == "REVIEW_REQUIRED":
        emit_bail("other", "PR requires human review approval before merge")
        raise _ReviewRequiredError("ci-gate: PR blocked by required human review")


def _poll_until_done(
    pr_url: str,
    timeout: int,
    interval: int,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None,
) -> tuple[list[dict[str, Any]], str]:
    """Poll PR checks until all are complete or timeout. Returns (checks, review_decision)."""
    deadline = time.time() + timeout
    review_decision = ""
    while True:
        if checks_getter is not None:
            checks, review_decision = checks_getter()
        else:
            status = get_pr_ci_status(pr_url)
            checks = status["checks"]
            review_decision = status["review_decision"]
        _bail_if_review_required(review_decision)
        if not checks or all(_is_done(c) for c in checks):
            return checks, review_decision
        if time.time() >= deadline:
            logger.info("ci-gate: poll timed out after %ds", timeout)
            return checks, review_decision
        logger.debug("ci-gate: checks still pending, sleeping %ds", interval)
        time.sleep(interval)


def _collect_failure_output(failed: list[dict[str, Any]]) -> str:
    """Concatenate log output from failing checks."""
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


def _escape_fmt(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


def run_wait_ci_stage(
    *,
    client: ClaudeClient,
    model: str | None,
    pr_url: str,
    artifacts_dir: pathlib.Path,
    code_style: str,
    max_attempts: int = 3,
    poll_timeout: int = 1200,
    poll_interval: int = 30,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None,
) -> None:
    """Wait for CI checks to pass, fixing failures up to max_attempts times.

    ``checks_getter`` is injectable for tests: a zero-argument callable that
    returns ``(checks_list, review_decision_string)``. Defaults to a real
    ``gh pr view`` call.

    Skips entirely when the PR has no checks configured. Bails when the PR is
    blocked by a required human review. Bails after max_attempts exhausted
    without a green run.
    """
    if checks_getter is not None:
        checks, review_decision = checks_getter()
    else:
        status = get_pr_ci_status(pr_url)
        checks = status["checks"]
        review_decision = status["review_decision"]

    _bail_if_review_required(review_decision)

    if not checks:
        logger.info("ci-gate: no CI checks found, skipping")
        return

    template = PROMPT_TEMPLATE_PATH.read_text(encoding="utf-8")
    bail_section = ""
    if os.environ.get("GR_ID"):
        bail_section = (
            "\n\nIf you cannot fix the failure, run:\n"
            '  `python -m gremlins.cli bail other "<one-line reason>"`\n'
            "before finishing."
        )

    _exhausted = False
    _agent_bailed = False
    _review_bailed = False
    try:
        for attempt in range(1, max_attempts + 1):
            logger.info(
                "ci-gate: attempt %d/%d — polling (timeout %ds)",
                attempt,
                max_attempts,
                poll_timeout,
            )
            try:
                final_checks, review_decision = _poll_until_done(
                    pr_url, poll_timeout, poll_interval, checks_getter
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

            if attempt == max_attempts:
                break

            failure_output = _collect_failure_output(failed)
            log_file = artifacts_dir / f"ci-attempt-{attempt}.log"
            log_file.write_text(failure_output, encoding="utf-8")

            fix_prompt = template.format(
                code_style=_escape_fmt(code_style),
                failure_output=failure_output,
                bail_section=bail_section,
            )
            client.run(
                fix_prompt,
                label=f"ci-fix-{attempt}",
                model=model,
                raw_path=artifacts_dir / f"stream-ci-fix-{attempt}.jsonl",
            )
            _agent_bailed = True
            check_bail(f"ci-fix-{attempt}")
            _agent_bailed = False

        _exhausted = True
        emit_bail("other", f"CI failed after {max_attempts} attempts")
        raise RuntimeError(f"ci-gate exhausted {max_attempts} attempts")
    except (SystemExit, Exception) as exc:
        if not _exhausted and not _agent_bailed and not _review_bailed:
            emit_bail("other", f"ci-gate failed: {exc}"[:200])
        raise
