"""Tests for gremlins.stages.wait_ci."""

import pathlib
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.wait_ci import run_wait_ci_stage

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


def _make_getter(responses: list[tuple[list[dict[str, Any]], str]]):
    """Return a checks_getter that yields successive (checks, review_decision) pairs."""
    it = iter(responses)

    def getter():
        return next(it)

    return getter


def test_no_checks_skips(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([], ""),
        ]
    )
    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        checks_getter=getter,
    )
    assert client.calls == []


def test_review_required_no_checks_bails(tmp_path: pathlib.Path) -> None:
    """REVIEW_REQUIRED is checked before the no-checks short-circuit."""
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([], "REVIEW_REQUIRED"),
        ]
    )
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci_stage(
            client=client,
            model="sonnet",
            pr_url=PR_URL,
            artifacts_dir=tmp_path,
            code_style="Be good.",
            checks_getter=getter,
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
    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        checks_getter=getter,
    )
    assert client.calls == []


def test_checks_pending_then_passing(tmp_path: pathlib.Path) -> None:
    """First poll returns pending check; second poll (within _poll_until_done) returns passing."""
    client = FakeClaudeClient(fixtures={})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_PENDING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        poll_interval=0,
        checks_getter=getter,
    )
    assert client.calls == []


def test_review_required_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([_PASSING_CHECK], "REVIEW_REQUIRED"),
        ]
    )
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci_stage(
            client=client,
            model="sonnet",
            pr_url=PR_URL,
            artifacts_dir=tmp_path,
            code_style="Be good.",
            checks_getter=getter,
        )
    assert client.calls == []


def test_review_required_after_fix_bails(tmp_path: pathlib.Path) -> None:
    """Fix commit dismisses approval: next poll returns REVIEW_REQUIRED → bail."""
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    getter = _make_getter(
        [
            ([_FAILING_CHECK], ""),  # startup: failing, not review required
            ([_FAILING_CHECK], ""),  # poll attempt 1: still failing
            (
                [_PASSING_CHECK],
                "REVIEW_REQUIRED",
            ),  # poll attempt 2: fix pushed, approval dismissed
        ]
    )
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci_stage(
            client=client,
            model="sonnet",
            pr_url=PR_URL,
            artifacts_dir=tmp_path,
            code_style="Be good.",
            poll_interval=0,
            checks_getter=getter,
        )
    assert len(client.calls) == 1
    assert client.calls[0].label == "ci-fix-1"


def test_review_required_while_pending_bails(tmp_path: pathlib.Path) -> None:
    """REVIEW_REQUIRED flip while checks are still pending bails immediately."""
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([_PENDING_CHECK], ""),  # startup: pending
            (
                [_PENDING_CHECK],
                "REVIEW_REQUIRED",
            ),  # poll: still pending, approval dismissed
        ]
    )
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        run_wait_ci_stage(
            client=client,
            model="sonnet",
            pr_url=PR_URL,
            artifacts_dir=tmp_path,
            code_style="Be good.",
            poll_interval=0,
            checks_getter=getter,
        )
    assert client.calls == []


def test_fix_on_failure_then_pass(tmp_path: pathlib.Path) -> None:
    """First poll fails, claude is invoked to fix, second attempt passes."""
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        poll_interval=0,
        checks_getter=getter,
    )
    assert len(client.calls) == 1
    assert client.calls[0].label == "ci-fix-1"


def test_exhausted_bails(tmp_path: pathlib.Path) -> None:
    """All 3 attempts fail → RuntimeError('ci-gate exhausted 3 attempts')."""
    client = FakeClaudeClient(
        fixtures={
            "ci-fix-1": MINIMAL_EVENTS,
            "ci-fix-2": MINIMAL_EVENTS,
        }
    )
    getter = _make_getter(
        [
            ([_FAILING_CHECK], ""),  # initial check
            ([_FAILING_CHECK], ""),  # poll attempt 1
            ([_FAILING_CHECK], ""),  # poll attempt 2
            ([_FAILING_CHECK], ""),  # poll attempt 3
        ]
    )
    with pytest.raises(RuntimeError, match="ci-gate exhausted 3 attempts"):
        run_wait_ci_stage(
            client=client,
            model="sonnet",
            pr_url=PR_URL,
            artifacts_dir=tmp_path,
            code_style="Be good.",
            poll_interval=0,
            checks_getter=getter,
        )
    # Two fix attempts (not three — last attempt doesn't call claude)
    fix_labels = [c.label for c in client.calls]
    assert fix_labels == ["ci-fix-1", "ci-fix-2"]


def test_timeout_counts_as_failed(tmp_path: pathlib.Path) -> None:
    """Timeout with pending checks: pending != failing, so stage passes without a fix call."""
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 3:
            return [_PENDING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        poll_timeout=0,  # instant timeout
        poll_interval=0,
        checks_getter=getter,
    )
    assert client.calls == []


def test_post_fix_waits_for_sha_propagation(tmp_path: pathlib.Path) -> None:
    """After a ci-fix push, stale passing checks from the old commit are ignored
    until headRefOid on GitHub catches up to the new commit SHA.

    Call sequence:
      1. initial check (run_wait_ci_stage entry): FAILING → proceed to loop
      2. attempt 1 poll: FAILING → all done, fix triggered
      3. attempt 2 poll: PASSING but old_sha → wait
      4. attempt 2 poll: PASSING and new_sha → accept
    """
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})

    check_call = [0]
    sha_call = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        check_call[0] += 1
        if check_call[0] <= 2:
            return [_FAILING_CHECK], ""  # initial + attempt-1 poll: failing
        # Post-fix polls: stale old-commit check reports passing right away
        return [_PASSING_CHECK], ""

    def head_sha_getter() -> str:
        sha_call[0] += 1
        # First poll after fix: GitHub hasn't propagated the push yet
        if sha_call[0] <= 1:
            return "old_sha"
        return "new_sha"

    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        poll_interval=0,
        checks_getter=getter,
        head_sha_getter=head_sha_getter,
        fix_sha_getter=lambda: "new_sha",
    )
    assert len(client.calls) == 1
    # head_sha_getter was called at least twice: once returning old_sha, once new_sha
    assert sha_call[0] >= 2


def test_post_fix_no_sha_available_falls_back(tmp_path: pathlib.Path) -> None:
    """When fix_sha_getter returns '' (e.g. not in a git repo), SHA check is
    skipped and the stage falls back to the original check-only logic."""
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})

    check_call = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        check_call[0] += 1
        if check_call[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Be good.",
        poll_interval=0,
        checks_getter=getter,
        fix_sha_getter=lambda: "",  # no git HEAD available
    )
    assert len(client.calls) == 1


def test_code_style_in_fix_prompt(tmp_path: pathlib.Path) -> None:
    """code_style content appears in the claude prompt sent for fixing."""
    client = FakeClaudeClient(
        fixtures={"ci-fix-1": MINIMAL_EVENTS, "ci-fix-2": MINIMAL_EVENTS}
    )
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    run_wait_ci_stage(
        client=client,
        model="sonnet",
        pr_url=PR_URL,
        artifacts_dir=tmp_path,
        code_style="Always write docstrings.",
        poll_interval=0,
        checks_getter=getter,
    )
    assert client.calls
    assert "Always write docstrings." in client.calls[0].prompt
