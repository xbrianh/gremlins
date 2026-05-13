"""Tests for gremlins.stages.github_wait_ci."""

import json
import pathlib
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.github_wait_ci import GitHubWaitCI

PR_URL = "https://github.com/owner/repo/pull/42"

_CI_PROMPT_PATH = (
    pathlib.Path(__file__).resolve().parent.parent
    / "gremlins"
    / "prompts"
    / "ci_fix.md"
)

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


def _make_stage(
    client: Any,
    tmp_path: Any,
    *,
    gr_id: Any = None,
    model: str = "sonnet",
    pr_branch: str | None = "test-pr-branch",
    **kwargs: Any,
) -> tuple[GitHubWaitCI, RuntimeState]:
    prompts = [_CI_PROMPT_PATH.read_text(encoding="utf-8")]
    stage = GitHubWaitCI("github-wait-ci", model, prompts, {}, pr_url=PR_URL, **kwargs)
    state = RuntimeState(
        data=StateData(gr_id=gr_id), client=client, session_dir=tmp_path
    )
    if pr_branch is not None:
        state.data.last_pr_branch = lambda: pr_branch  # type: ignore[method-assign]
    return stage, state


def _make_getter(responses: list[tuple[list[dict[str, Any]], str]]):
    it = iter(responses)

    def getter():
        return next(it)

    return getter


def test_no_checks_skips(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([], "")])
    stage, state = _make_stage(
        client, tmp_path, startup_grace_secs=0, checks_getter=getter
    )
    stage.run(state)
    assert client.calls == []


def test_review_required_no_checks_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([], "REVIEW_REQUIRED")])
    stage, state = _make_stage(client, tmp_path, checks_getter=getter)
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        stage.run(state)
    assert client.calls == []


def test_all_checks_passing_returns(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [
            ([_PASSING_CHECK], "APPROVED"),
            ([_PASSING_CHECK], "APPROVED"),
        ]
    )
    stage, state = _make_stage(client, tmp_path, checks_getter=getter)
    stage.run(state)
    assert client.calls == []


def test_checks_pending_then_passing(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_PENDING_CHECK], ""
        return [_PASSING_CHECK], ""

    stage, state = _make_stage(client, tmp_path, poll_interval=0, checks_getter=getter)
    stage.run(state)
    assert client.calls == []


def test_review_required_bails(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([_PASSING_CHECK], "REVIEW_REQUIRED")])
    stage, state = _make_stage(client, tmp_path, checks_getter=getter)
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        stage.run(state)
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
    stage, state = _make_stage(client, tmp_path, poll_interval=0, checks_getter=getter)
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        stage.run(state)
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
    stage, state = _make_stage(client, tmp_path, poll_interval=0, checks_getter=getter)
    with pytest.raises(RuntimeError, match="PR blocked by required human review"):
        stage.run(state)
    assert client.calls == []


def test_fix_on_failure_then_pass(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] <= 2:
            return [_FAILING_CHECK], ""
        return [_PASSING_CHECK], ""

    stage, state = _make_stage(client, tmp_path, poll_interval=0, checks_getter=getter)
    stage.run(state)
    assert len(client.calls) == 1
    assert client.calls[0].label == "ci-fix-1"


def test_ci_fix_prompt_contains_pr_branch(
    tmp_path: pathlib.Path, make_state_dir
) -> None:
    gr_id = "test-ci-fix-branch"
    state_dir = make_state_dir(gr_id)
    branch = "issue-42-my-feature"
    (state_dir / "state.json").write_text(
        json.dumps(
            {"id": gr_id, "stage": "", "artifacts": [{"type": "pr", "branch": branch}]}
        )
    )
    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    getter = _make_getter(
        [([_FAILING_CHECK], ""), ([_FAILING_CHECK], ""), ([_PASSING_CHECK], "")]
    )
    stage, state = _make_stage(
        client,
        tmp_path,
        gr_id=gr_id,
        poll_interval=0,
        checks_getter=getter,
        pr_branch=None,
    )
    stage.run(state)
    assert len(client.calls) == 1
    assert branch in client.calls[0].prompt


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
    stage, state = _make_stage(client, tmp_path, poll_interval=0, checks_getter=getter)
    with pytest.raises(RuntimeError, match="ci-gate exhausted 3 attempts"):
        stage.run(state)
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

    stage, state = _make_stage(
        client, tmp_path, poll_timeout=0, poll_interval=0, checks_getter=getter
    )
    stage.run(state)
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

    stage, state = _make_stage(
        client,
        tmp_path,
        poll_interval=0,
        checks_getter=getter,
        head_sha_getter=head_sha_getter,
        fix_sha_getter=lambda: "new_sha",
    )
    stage.run(state)
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

    stage, state = _make_stage(
        client,
        tmp_path,
        poll_interval=0,
        checks_getter=getter,
        fix_sha_getter=lambda: "",
    )
    stage.run(state)
    assert len(client.calls) == 1


def test_grace_period_waits_for_checks_to_appear(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] == 1:
            return [], ""
        return [_PASSING_CHECK], ""

    stage, state = _make_stage(
        client, tmp_path, poll_interval=0, startup_grace_secs=60, checks_getter=getter
    )
    stage.run(state)
    assert client.calls == []
    assert call_count[0] >= 2


def test_no_checks_after_grace_skips(tmp_path: pathlib.Path) -> None:
    """Grace period elapses with no checks appearing — should skip without invoking agent."""
    client = FakeClaudeClient(fixtures={})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        return [], ""

    stage, state = _make_stage(
        client, tmp_path, poll_interval=0, startup_grace_secs=1, checks_getter=getter
    )
    stage.run(state)
    assert client.calls == []
    assert call_count[0] >= 2


def test_poll_empty_mid_run_continues_polling(tmp_path: pathlib.Path) -> None:
    client = FakeClaudeClient(fixtures={})
    call_count = [0]

    def getter() -> tuple[list[dict[str, Any]], str]:
        call_count[0] += 1
        if call_count[0] == 1:
            return [_PENDING_CHECK], ""
        if call_count[0] == 2:
            return [], ""
        return [_PASSING_CHECK], ""

    stage, state = _make_stage(
        client, tmp_path, poll_interval=0, startup_grace_secs=0, checks_getter=getter
    )
    stage.run(state)
    assert client.calls == []
    assert call_count[0] >= 3


def test_review_required_emits_bail_to_state(
    tmp_path: pathlib.Path, make_state_dir
) -> None:
    import gremlins.executor.state as state_mod

    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    attempt = "github-wait-ci-test"
    state_mod.StateData.load(gr_id).patch(attempt=attempt)
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter([([], "REVIEW_REQUIRED")])
    stage, state = _make_stage(client, tmp_path, gr_id=gr_id, checks_getter=getter)
    state.data.attempt = attempt
    with pytest.raises(RuntimeError):
        stage.run(state)
    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"


def test_empty_pr_branch_bails(tmp_path: pathlib.Path, make_state_dir) -> None:
    import gremlins.executor.state as state_mod

    gr_id = "test-empty-branch"
    state_dir = make_state_dir(gr_id)
    attempt = "github-wait-ci-test"
    # PR artifact with no branch — simulates open-pr appending without a known branch
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "attempt": attempt,
                "artifacts": [{"type": "pr", "branch": ""}],
            }
        )
    )
    state_mod.StateData.load(gr_id).patch(attempt=attempt)
    client = FakeClaudeClient(fixtures={})
    getter = _make_getter(
        [([_FAILING_CHECK], ""), ([_FAILING_CHECK], ""), ([_PASSING_CHECK], "")]
    )
    stage, state = _make_stage(
        client,
        tmp_path,
        gr_id=gr_id,
        poll_interval=0,
        startup_grace_secs=0,
        checks_getter=getter,
        pr_branch=None,
    )
    state.data.attempt = attempt
    stage.run(state)
    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"
    assert client.calls == []


def test_check_bail_raises_from_state(tmp_path: pathlib.Path, make_state_dir) -> None:
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)
    attempt = "ci-fix-test-attempt"
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id, "attempt": attempt}))
    # Write bail file as the agent would have
    (state_dir / f"bail_{attempt}.json").write_text(json.dumps({"class": "other"}))

    client = FakeClaudeClient(fixtures={"ci-fix-1": MINIMAL_EVENTS})
    getter = _make_getter([([_FAILING_CHECK], ""), ([_FAILING_CHECK], "")])
    stage, state = _make_stage(
        client,
        tmp_path,
        gr_id=gr_id,
        poll_interval=0,
        poll_timeout=0,
        startup_grace_secs=0,
        checks_getter=getter,
    )
    state.data.attempt = attempt
    with pytest.raises(RuntimeError, match="bailed"):
        stage.run(state)
