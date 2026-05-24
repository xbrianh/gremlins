"""Tests for gremlins.stages.github_wait_copilot."""

from __future__ import annotations

import asyncio
import pathlib
from collections.abc import Callable

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.github_wait_copilot import GitHubWaitCopilot


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    repo: str = "owner/repo",
    pr_num: str = "42",
    timeout: int = 600,
    interval: int = 0,
    max_poll_failures: int = 5,
    review_checker: Callable[[], str | None] | None = None,
    gremlin_id: str | None = None,
) -> tuple[GitHubWaitCopilot, RuntimeState]:
    stage = GitHubWaitCopilot(
        "github-wait-copilot",
        [],
        {},
        pr_num=pr_num,
        timeout=timeout,
        interval=interval,
        max_poll_failures=max_poll_failures,
        review_checker=review_checker,
    )
    state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=FakeClaudeClient(fixtures={}),
        session_dir=tmp_path,
        repo=repo,
    )
    return stage, state


def test_returns_review_state_immediately(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Done

    call_count = [0]

    def checker() -> str | None:
        call_count[0] += 1
        return "APPROVED"

    stage, state = _make_stage(tmp_path, review_checker=checker)
    result = asyncio.run(stage.run(state))
    assert result == Done()
    assert call_count[0] == 1


def test_polls_until_review_arrives(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Done

    call_count = [0]

    def checker() -> str | None:
        call_count[0] += 1
        return "CHANGES_REQUESTED" if call_count[0] >= 3 else None

    stage, state = _make_stage(tmp_path, review_checker=checker)
    result = asyncio.run(stage.run(state))
    assert result == Done()
    assert call_count[0] == 3


def test_timeout_bails(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Bail

    stage, state = _make_stage(tmp_path, timeout=0, review_checker=lambda: None)
    with pytest.raises(Bail) as exc_info:
        asyncio.run(stage.run(state))
    assert "timed out" in exc_info.value.reason


def test_no_pr_num_raises(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path, pr_num="")
    with pytest.raises(RuntimeError, match="no pr_url in state.json"):
        asyncio.run(stage.run(state))


def test_always_failing_checker_bails_with_error(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Bail

    def checker() -> str | None:
        raise RuntimeError("gh api reviews failed (exit 1): Bad credentials")

    stage, state = _make_stage(tmp_path, review_checker=checker, max_poll_failures=5)
    with pytest.raises(Bail) as exc_info:
        asyncio.run(stage.run(state))
    assert "Bad credentials" in exc_info.value.reason
    assert "timed out" not in exc_info.value.reason


def test_transient_failures_then_success(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Done

    call_count = [0]

    def checker() -> str | None:
        call_count[0] += 1
        if call_count[0] <= 3:
            raise RuntimeError("transient error")
        return "APPROVED"

    stage, state = _make_stage(tmp_path, review_checker=checker, max_poll_failures=5)
    result = asyncio.run(stage.run(state))
    assert result == Done()
    assert call_count[0] == 4


def test_timeout_message_includes_context(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Bail

    call_count = [0]

    def checker() -> str | None:
        call_count[0] += 1
        return None

    stage, state = _make_stage(tmp_path, timeout=0, review_checker=checker)
    with pytest.raises(Bail) as exc_info:
        asyncio.run(stage.run(state))
    assert "polls=" in exc_info.value.reason
    assert "last_error=" in exc_info.value.reason
