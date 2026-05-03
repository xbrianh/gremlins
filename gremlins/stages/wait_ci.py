"""CI gate stage for the gh pipeline."""

from __future__ import annotations

import dataclasses
import logging
import time
from collections.abc import Callable
from typing import Any

from ..gh_utils import fetch_check_run_logs, get_pr_ci_status
from ..git import git_head
from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from ..state import check_bail, emit_bail
from .context import StageContext

logger = logging.getLogger(__name__)

_FAILING_CONCLUSIONS = frozenset({"FAILURE", "ERROR", "TIMED_OUT", "CANCELLED"})
_PENDING_STATES = frozenset({"EXPECTED", "PENDING"})


@dataclasses.dataclass
class WaitCiOptions:
    model: str | None
    pr_url: str
    code_style: str
    max_attempts: int = 3
    poll_timeout: int = 1200
    poll_interval: int = 30
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None = None
    head_sha_getter: Callable[[], str] | None = None
    fix_sha_getter: Callable[[], str] | None = None


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
    required_sha: str = "",
    head_sha_getter: Callable[[], str] | None = None,
) -> tuple[list[dict[str, Any]], str]:
    """Poll PR checks until all are complete or timeout."""
    deadline = time.time() + timeout
    review_decision = ""
    while True:
        head_sha = ""
        if checks_getter is not None:
            checks, review_decision = checks_getter()
            if head_sha_getter is not None:
                head_sha = head_sha_getter()
        else:
            status = get_pr_ci_status(pr_url)
            checks = status["checks"]
            review_decision = status["review_decision"]
            head_sha = status["head_sha"]

        _bail_if_review_required(review_decision)

        if required_sha and head_sha and head_sha != required_sha:
            if time.time() >= deadline:
                raise RuntimeError(
                    f"ci-gate: timed out waiting for GitHub to reflect pushed SHA "
                    f"{required_sha[:8]} (still showing {head_sha[:8]}) after {timeout}s"
                )
            logger.debug(
                "ci-gate: PR head %s != expected %s, waiting for push to propagate",
                head_sha[:8],
                required_sha[:8],
            )
            time.sleep(interval)
            continue

        if not checks or all(_is_done(c) for c in checks):
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


def _escape_fmt(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


def run(ctx: StageContext, options: WaitCiOptions) -> None:
    """Wait for CI checks to pass, fixing failures up to max_attempts times."""
    if options.checks_getter is not None:
        checks, review_decision = options.checks_getter()
    else:
        status = get_pr_ci_status(options.pr_url)
        checks = status["checks"]
        review_decision = status["review_decision"]

    _bail_if_review_required(review_decision)

    if not checks:
        logger.info("ci-gate: no CI checks found, skipping")
        return

    template = load_prompts([BUNDLED_PROMPT_DIR / "ci_fix.md"])
    bail_section = ""
    if ctx.gr_id:
        bail_section = (
            "\n\nIf you cannot fix the failure, run:\n"
            '  `python -m gremlins.cli bail other "<one-line reason>"`\n'
            "before finishing."
        )

    _exhausted = False
    _agent_bailed = False
    _review_bailed = False
    fix_sha = ""
    try:
        for attempt in range(1, options.max_attempts + 1):
            logger.info(
                "ci-gate: attempt %d/%d — polling (timeout %ds)",
                attempt,
                options.max_attempts,
                options.poll_timeout,
            )
            try:
                final_checks, review_decision = _poll_until_done(
                    options.pr_url,
                    options.poll_timeout,
                    options.poll_interval,
                    options.checks_getter,
                    required_sha=fix_sha,
                    head_sha_getter=options.head_sha_getter,
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

            if attempt == options.max_attempts:
                break

            failure_output = _collect_failure_output(failed)
            log_file = ctx.session_dir / f"ci-attempt-{attempt}.log"
            log_file.write_text(failure_output, encoding="utf-8")

            fix_prompt = template.format(
                code_style=_escape_fmt(options.code_style),
                failure_output=failure_output,
                bail_section=bail_section,
            )
            ctx.client.run(
                fix_prompt,
                label=f"ci-fix-{attempt}",
                model=options.model,
                raw_path=ctx.session_dir / f"stream-ci-fix-{attempt}.jsonl",
            )
            _agent_bailed = True
            check_bail(f"ci-fix-{attempt}")
            _agent_bailed = False

            _get_sha = options.fix_sha_getter if options.fix_sha_getter is not None else git_head
            fix_sha = _get_sha()

        _exhausted = True
        emit_bail("other", f"CI failed after {options.max_attempts} attempts")
        raise RuntimeError(f"ci-gate exhausted {options.max_attempts} attempts")
    except (SystemExit, Exception) as exc:
        if not _exhausted and not _agent_bailed and not _review_bailed:
            emit_bail("other", f"ci-gate failed: {exc}"[:200])
        raise
