"""Tests for gremlins.stages.wait_ci."""

import pathlib
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.context import StageContext
from gremlins.stages.wait_ci import WaitCiOptions
from gremlins.stages.wait_ci import run as run_wait_ci

PR_URL = "https://github.com/owner/repo/pull/42"

_PASSING_CHECK = {
    "__typename": "CheckRun",
    "name": "tests",
    "status": "COMPLETED",
    "conclusion": "SUCCESS",
    "detailsUrl": "",
}

_PENDING_CHECK = {
    "__typename": "CheckRun",
    "name": "tests",
    "status": "IN_PROGRESS",
    "conclusion": None,
    "detailsUrl": "",
}

_FAILING_CHECK = {
    "__typename": "CheckRun",
    "name": "tests",
    "status": "COMPLETED",
    "conclusion": "FAILURE",
    "detailsUrl": "",
}


def _make_ctx(client: Any, tmp_path: Any) -> StageContext:
    return StageContext(client=client, session_dir=tmp_path, gr_id=None)


def _make_getter(responses: list[tuple[list[dict[str, Any]], str]]):
    it = iter(responses)

    def getter():
        return next(it)

    return getter


def test_no_checks_skips(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([], "")])
    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(model="sonnet", pr_url=PR_URL, code_style="Be good.", checks_getter=getter),
    )
    assert client.calls == []


def test_review_required_no_checks_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([], "REVIEW_REQUIRED")])
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci(
            _make_ctx(client, tmp_path),
            WaitCiOptions(model="sonnet", pr_url=PR_URL, code_style="Be good.", checks_getter=getter),
        )
    assert client.calls == []


def test_all_checks_passing_returns(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([_PASSING_CHECK], "APPROVED"),
            ([_PASSING_CHECK], "APPROVED"),
        ]
    )
    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(model="sonnet", pr_url=PR_URL, code_style="Be good.", checks_getter=getter),
    )
    assert client.calls == []


def test_checks_pending_then_passing(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_PENDING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(
            model="sonnet", pr_url=PR_URL, code_style="Be good.",
            poll_interval=0, checks_getter=getter,
        ),
    )
    assert client.calls == []


def test_review_required_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([_PASSING_CHECK], "REVIEW_REQUIRED")])
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci(
            _make_ctx(client, tmp_path),
            WaitCiOptions(model="sonnet", pr_url=PR_URL, code_style="Be good.", checks_getter=getter),
        )
    assert client.calls == []


def test_review_required_after_fix_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    getter = _make_getter(
        [
            ([_FAILING_CHECK], ""),
            ([_FAILING_CHECK], ""),
            ([_PASSING_CHECK], "REVIEW_REQUIRED"),
        ]
    )
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci(
            _make_ctx(client, tmp_path),
            WaitCiOptions(
                model="sonnet", pr_url=PR_URL, code_style="Be good.",
                poll_interval=0, checks_getter=getter,
            ),
        )
    assert len(client.calls) == 1
    assert client.calls[0].label == "ci-fix-1"


def test_review_required_while_pending_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([_PENDING_CHECK], ""),
            ([_PENDING_CHECK], "REVIEW_REQUIRED"),
        ]
    )
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci(
            _make_ctx(client, tmp_path),
            WaitCiOptions(
                model="sonnet", pr_url=PR_URL, code_style="Be good.",
                poll_interval=0, checks_getter=getter,
            ),
        )
    assert client.calls == []


def test_fix_on_failure_then_pass(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(
            model="sonnet", pr_url=PR_URL, code_style="Be good.",
            poll_interval=0, checks_getter=getter,
        ),
    )
    assert len(client.calls) == 1
    assert client.calls[0].label == "ci-fix-1"


def test_exhausted_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(
        fixtures={
            "ci-fix-1": MINIMAL_EVENTS,
            "ci-fix-2": MINIMAL_EVENTS,
        }
    )
    getter = _make_getter(
        [
            ([_FAILING_CHECK], ""),
            ([_FAILING_CHECK], ""),
            ([_FAILING_CHECK], ""),
            ([_FAILING_CHECK], ""),
        ]
    )
    with pytest.raises(RuntimeError, match="ci-gate exhausted 3 attempts"):
        run_wait_ci(
            _make_ctx(client, tmp_path),
            WaitCiOptions(
                model="sonnet", pr_url=PR_URL, code_style="Be good.",
                poll_interval=0, checks_getter=getter,
            ),
        )
    fix_labels = [c.label for c in client.calls]
    assert fix_labels == ["ci-fix-1", "ci-fix-2"]


def test_timeout_counts_as_failed(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 3:
            return [_PENDING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(
            model="sonnet", pr_url=PR_URL, code_style="Be good.",
            poll_timeout=0, poll_interval=0, checks_getter=getter,
        ),
    )
    assert client.calls == []


def test_post_fix_waits_for_sha_propagation(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})

    check_call = [0]
    sha_call = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        check_call[0] += 1
        if check_call[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    def head_sha_getter() -> str:
        sha_call[0] += 1
        if sha_call[0] <= 1:
            return "old_sha"
        return "new_sha"

    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(
            model="sonnet", pr_url=PR_URL, code_style="Be good.",
            poll_interval=0, checks_getter=getter,
            head_sha_getter=head_sha_getter, fix_sha_getter=lambda: "new_sha",
        ),
    )
    assert len(client.calls) == 1
    assert sha_call[0] >= 2


def test_post_fix_no_sha_available_falls_back(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})

    check_call = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        check_call[0] += 1
        if check_call[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(
            model="sonnet", pr_url=PR_URL, code_style="Be good.",
            poll_interval=0, checks_getter=getter, fix_sha_getter=lambda: "",
        ),
    )
    assert len(client.calls) == 1


def test_code_style_in_fix_prompt(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(
        fixtures={"ci-fix-1": MINIMAL_EVENTS, "ci-fix-2": MINIMAL_EVENTS}
    )
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci(
        _make_ctx(client, tmp_path),
        WaitCiOptions(
            model="sonnet", pr_url=PR_URL, code_style="Always write docstrings.",
            poll_interval=0, checks_getter=getter,
        ),
    )
    assert client.calls
    assert "Always write docstrings." in client.calls[0].prompt
