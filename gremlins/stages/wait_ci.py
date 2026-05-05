"""CI gate stage for the gh pipeline."""

from __future__ import annotations

import dataclasses
import logging
import pathlib
import time
from collections.abc import Callable
from typing import Any

from gremlins.gh_utils import fetch_check_run_logs, get_pr_ci_status
from gremlins.git import git_head
from gremlins.prompts import load_prompts
from gremlins.stages.context import StageContext
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail, emit_bail

logger = logging.getLogger(__name__)

_PROMPT = pathlib.Path(__file__).resolve().parent / "ci_fix.md"

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
    startup_grace_secs: int = 60
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


def _fetch_checks(
    pr_url: str,
    checks_getter: Callable[[], tuple[list[dict[str, Any]], str]] | None,
) -> tuple[list[dict[str, Any]], str]:
    if checks_getter is not None:
        return checks_getter()
    status = get_pr_ci_status(pr_url)
    return status["checks"], status["review_decision"]


def _wait_for_checks(
    options: WaitCiOptions,
    grace_secs: int,
) -> tuple[list[dict[str, Any]], str]:
    deadline = time.time() + grace_secs
    review_decision = ""
    while True:
        checks, review_decision = _fetch_checks(options.pr_url, options.checks_getter)
        if checks or review_decision == "REVIEW_REQUIRED" or time.time() >= deadline:
            return checks, review_decision
        time.sleep(options.poll_interval)


def _bail_if_review_required(
    gr_id: str | None, decision: str, child_key: str | None = None
) -> None:
    if decision == "REVIEW_REQUIRED":
        emit_bail(gr_id, "other", "PR requires human review approval before merge", child_key=child_key)
        raise _ReviewRequiredError("ci-gate: PR blocked by required human review")


def _poll_until_done(
    gr_id: str | None,
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

        _bail_if_review_required(gr_id, review_decision)

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


def _escape_fmt(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


def run(ctx: StageContext, options: WaitCiOptions) -> None:
    """Wait for CI checks to pass, fixing failures up to max_attempts times."""
    checks, review_decision = _wait_for_checks(options, options.startup_grace_secs)
    _bail_if_review_required(ctx.gr_id, review_decision, child_key=ctx.child_key)

    if not checks:
        logger.info(
            "ci-gate: PR has no check-runs after %ds, skipping",
            options.startup_grace_secs,
        )
        return

    template = load_prompts([_PROMPT])
    bail_section = ""
    if ctx.gr_id:
        bail_section = (
            "\n\nIf you cannot fix the failure, run:\n"
            '  `python -m gremlins.bail other "<one-line reason>"`\n'
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
                    ctx.gr_id,
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
            check_bail(ctx.gr_id, f"ci-fix-{attempt}", child_key=ctx.child_key)
            _agent_bailed = False

            _get_sha = (
                options.fix_sha_getter
                if options.fix_sha_getter is not None
                else git_head
            )
            fix_sha = _get_sha()

        _exhausted = True
        emit_bail(
            ctx.gr_id, "other", f"CI failed after {options.max_attempts} attempts",
            child_key=ctx.child_key,
        )
        raise RuntimeError(f"ci-gate exhausted {options.max_attempts} attempts")
    except (SystemExit, Exception) as exc:
        if not _exhausted and not _agent_bailed and not _review_bailed:
            emit_bail(ctx.gr_id, "other", f"ci-gate failed: {exc}"[:200], child_key=ctx.child_key)
        raise


register_stage("wait-ci", run)
