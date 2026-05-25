"""Tests for gremlins.stages.implement."""

from __future__ import annotations

import asyncio
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.implement import Implement
from gremlins.utils.git import (
    DivergentHead,
    EmptyImpl,
    HeadAdvanced,
    PreImplState,
)

_TEMPLATE_LOCAL = "plan: {plan_text}{spec_block}"
_TEMPLATE_GH = "{spec_block}{plan_source_label}{plan_text}{plan_location_note}"


@pytest.fixture(autouse=True)
def _mock_rev_parse(monkeypatch):
    monkeypatch.setattr(
        "gremlins.stages.implement.proc.run_or_raise",
        lambda cmd, **kwargs: cmd[-1],
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.git_utils.in_git_repo",
        lambda **kwargs: True,
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.snapshot_head_before",
        lambda **kwargs: "pre-sha",
    )
    monkeypatch.setattr(
        "gremlins.artifacts.registry.git_utils.head_sha",
        lambda **kwargs: "post-sha",
    )


def _make_state(
    tmp_path: pathlib.Path,
    *,
    plan_text: str = "do the thing",
    spec_text: str = "",
    prompts: list[str] | None = None,
    issue_num: str = "",
    base_ref_sha: str = "abc123",
) -> tuple[Implement, RuntimeState]:
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    stage = Implement("implement", prompts or [], {})
    client = FakeClaudeClient(fixtures={"implement": MINIMAL_EVENTS})
    state = build_state(
        data=StateData(issue_num=issue_num, base_ref_sha=base_ref_sha),
        client=client,
        session_dir=session_dir,
        artifacts=ArtifactRegistry(session_dir, cwd=tmp_path),
    )
    (session_dir / "plan.md").write_text(plan_text, encoding="utf-8")
    if spec_text:
        (session_dir / "spec.md").write_text(spec_text, encoding="utf-8")
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


def test_binds_commit_range_on_head_advanced(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_GH])
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=3),
    ):
        asyncio.run(stage.run(state))
    assert state.artifacts.produced("impl-commits")


def test_no_commit_range_bound_on_empty_impl_without_prior(
    tmp_path: pathlib.Path,
) -> None:
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_GH])
    with (
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=EmptyImpl(),
        ),
        pytest.raises(RuntimeError, match="no committed work"),
    ):
        asyncio.run(stage.run(state))
    assert not state.artifacts.produced("impl-commits")


def test_binds_commit_range_with_worktree(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_GH])
    state.worktree = tmp_path
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=1),
    ):
        asyncio.run(stage.run(state))
    assert state.artifacts.produced("impl-commits")


def test_empty_impl_with_prior_commit_range_does_not_raise(
    tmp_path: pathlib.Path,
) -> None:
    from gremlins.artifacts.uri import Uri

    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_GH])
    state.artifacts.bind("impl-commits", Uri.parse("git://range/aaa..bbb"))
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=EmptyImpl(),
    ):
        asyncio.run(stage.run(state))


def test_implement_forwards_options_via_agent(tmp_path: pathlib.Path) -> None:
    """capture_events is forwarded to run_agent via Agent."""
    stage, state = _make_state(tmp_path, prompts=[_TEMPLATE_LOCAL])
    with patch(
        "gremlins.stages.implement.classify_impl_outcome",
        return_value=HeadAdvanced(commit_count=1),
    ):
        asyncio.run(stage.run(state))
    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "implement"
    assert call.capture_events is True
