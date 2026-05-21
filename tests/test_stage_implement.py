"""Tests for gremlins.stages.implement."""

from __future__ import annotations

import asyncio
import pathlib
from unittest.mock import MagicMock, patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.implement import Implement
from gremlins.utils.git import DivergentHead, EmptyImpl, HeadAdvanced, PreImplState

_TEMPLATE_LOCAL = "plan: {plan_text}{spec_block}"
_TEMPLATE_GH = "{spec_block}{plan_source_label}{plan_text}{plan_location_note}"

_FAKE_PRE = PreImplState(head="abc123")


def _make_state(
    tmp_path: pathlib.Path,
    *,
    plan_text: str = "do the thing",
    spec_text: str = "",
    prompts: list[str] | None = None,
    issue_num: str = "",
) -> tuple[Implement, RuntimeState]:
    stage = Implement("implement", prompts or [], {})
    client = FakeClaudeClient(fixtures={"implement": MINIMAL_EVENTS})
    state = RuntimeState(
        data=StateData(issue_num=issue_num),
        client=client,
        session_dir=tmp_path,
    )
    (tmp_path / "plan.md").write_text(plan_text, encoding="utf-8")
    if spec_text:
        (tmp_path / "spec.md").write_text(spec_text, encoding="utf-8")
    return stage, state


def test_local_git_succeeds_on_head_advanced(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=2),
        ),
    ):
        asyncio.run(stage.run(state))
    assert len(state.client.calls) == 1


def test_local_git_raises_on_empty_impl(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=EmptyImpl(),
        ),
        pytest.raises(RuntimeError, match="no committed work"),
    ):
        asyncio.run(stage.run(state))


def test_gh_calls_claude_with_plan_text(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, plan_text="issue body here", prompts=[_TEMPLATE_GH]
    )
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
    ):
        asyncio.run(stage.run(state))
    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "implement"
    assert "issue body here" in call.prompt


def test_gh_plan_source_label_with_issue_num(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, plan_text="body", prompts=[_TEMPLATE_GH], issue_num="99"
    )
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
    ):
        asyncio.run(stage.run(state))
    prompt = state.client.calls[0].prompt
    assert "from the GitHub issue" in prompt


def test_gh_plan_source_label_without_issue_num(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
    ):
        asyncio.run(stage.run(state))
    prompt = state.client.calls[0].prompt
    assert "below" in prompt


def test_local_git_raises_on_divergent_head(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=DivergentHead(pre_head="abc123", post_head="def456"),
        ),
        pytest.raises(RuntimeError, match="diverged"),
    ):
        asyncio.run(stage.run(state))


def test_raises_on_empty_impl(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=EmptyImpl(),
        ),
        pytest.raises(RuntimeError, match="no committed work"),
    ):
        asyncio.run(stage.run(state))


def test_raises_on_divergent_head(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=DivergentHead(pre_head="abc123", post_head="def456"),
        ),
        pytest.raises(RuntimeError, match="diverged"),
    ):
        asyncio.run(stage.run(state))


def test_resume_uses_persisted_pre_impl_head(tmp_path: pathlib.Path) -> None:
    """On a resume, pre_impl_head in state must be used; record_pre_impl_state must not be called."""
    stage, state = _make_state(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    state.data.pre_impl_head = "deadbeef"

    record_mock = MagicMock(return_value=_FAKE_PRE)

    with (
        patch("gremlins.stages.implement.record_pre_impl_state", record_mock),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=3),
        ),
    ):
        asyncio.run(stage.run(state))

    record_mock.assert_not_called()
    assert state.data.pre_impl_head == "", "pre_impl_head must be cleared on success"


def test_resume_retains_pre_impl_head_on_failure(tmp_path: pathlib.Path) -> None:
    """pre_impl_head must not be cleared when the resumed run produces EmptyImpl."""
    stage, state = _make_state(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    state.data.pre_impl_head = "deadbeef"

    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=EmptyImpl(),
        ),
        pytest.raises(RuntimeError, match="no committed work"),
    ):
        asyncio.run(stage.run(state))

    assert state.data.pre_impl_head == "deadbeef", (
        "pre_impl_head must survive a failed resumed run"
    )


def test_run_does_not_access_pipeline_data(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])

    def _raise(self: object) -> None:
        raise AssertionError("pipeline_data accessed")

    with (
        patch.object(type(state), "pipeline_data", property(_raise)),
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
    ):
        asyncio.run(stage.run(state))
