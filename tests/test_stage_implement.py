"""Tests for gremlins.stages.implement."""

from __future__ import annotations

import asyncio
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.implement import Implement
from gremlins.utils.git import DivergentHead, EmptyImpl, HeadAdvanced, PreImplState

_TEMPLATE_LOCAL = "plan: {plan_text}{spec_block}"
_TEMPLATE_GH = "{spec_block}{plan_source_label}{plan_text}{plan_location_note}"


def _make_state(
    tmp_path: pathlib.Path,
    *,
    plan_text: str = "do the thing",
    spec_text: str = "",
    prompts: list[str] | None = None,
    issue_num: str = "",
    base_ref_sha: str = "abc123",
) -> tuple[Implement, RuntimeState]:
    stage = Implement("implement", prompts or [], {})
    client = FakeClaudeClient(fixtures={"implement": MINIMAL_EVENTS})
    state = RuntimeState(
        data=StateData(issue_num=issue_num, base_ref_sha=base_ref_sha),
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
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=2),
    ):
        asyncio.run(stage.run(state))
    assert len(state.client.calls) == 1


def test_local_git_raises_on_empty_impl(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])
    with (
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
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=1),
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
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=1),
    ):
        asyncio.run(stage.run(state))
    prompt = state.client.calls[0].prompt
    assert "from the GitHub issue" in prompt


def test_gh_plan_source_label_without_issue_num(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=1),
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
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=DivergentHead(pre_head="abc123", post_head="def456"),
        ),
        pytest.raises(RuntimeError, match="diverged"),
    ):
        asyncio.run(stage.run(state))


def test_base_ref_sha_used_as_baseline(tmp_path: pathlib.Path) -> None:
    """base_ref_sha is the pre-impl baseline; resume works without pre_impl_head."""
    stage, state = _make_state(
        tmp_path, plan_text="body", prompts=[_TEMPLATE_GH], base_ref_sha="deadbeef"
    )
    captured: list[PreImplState] = []

    def _capture(pre: PreImplState, **kwargs: object) -> HeadAdvanced:
        captured.append(pre)
        return HeadAdvanced(commit_count=3)

    with patch("gremlins.stages.implement.classify_impl_outcome", _capture):
        asyncio.run(stage.run(state))

    assert len(captured) == 1
    assert captured[0].head == "deadbeef"


def test_run_does_not_access_pipeline_data(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])

    def _raise(self: object) -> None:
        raise AssertionError("pipeline_data accessed")

    with (
        patch.object(type(state), "pipeline_data", property(_raise)),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
    ):
        asyncio.run(stage.run(state))
