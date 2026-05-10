"""Tests for gremlins.stages.implement."""

from __future__ import annotations

import json
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.git import DivergentHead, EmptyImpl, HeadAdvanced, PreImplState
from gremlins.schema import PipelineDef as _PipelineDef
from gremlins.schema import StageEntry as _StageEntry
from gremlins.stages.base import RuntimeState
from gremlins.stages.implement import Implement


def _gh_pipeline() -> _PipelineDef:
    return _PipelineDef(
        name="test",
        path=pathlib.Path("."),
        stages=[
            _StageEntry(
                name="open-github-pr",
                type="open-github-pr",
                client=None,
                prompts=[],
                options={},
            )
        ],
    )


_TEMPLATE_LOCAL = "plan: {plan_text}{spec_block}{impl_commit_instr}"
_TEMPLATE_GH = "{spec_block}{plan_source_label}{issue_body}{plan_location_note}"

_FAKE_PRE = PreImplState(head="abc123", branch="main")


def _make_state(
    tmp_path: pathlib.Path,
    *,
    plan_text: str = "do the thing",
    is_git: bool = True,
    spec_text: str = "",
    prompts: list[str] | None = None,
    is_gh: bool = False,
) -> tuple[Implement, RuntimeState]:
    stage = Implement("implement", "sonnet", prompts or [], {})
    client = FakeClaudeClient(fixtures={"implement": MINIMAL_EVENTS})
    pipeline_data = _gh_pipeline() if is_gh else None
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id=None,
        is_git=is_git,
        pipeline_data=pipeline_data,
    )
    (tmp_path / "plan.md").write_text(plan_text, encoding="utf-8")
    if spec_text:
        (tmp_path / "spec.md").write_text(spec_text, encoding="utf-8")
    return stage, state


def test_local_calls_claude(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(
        tmp_path,
        is_git=False,
        prompts=[_TEMPLATE_LOCAL],
    )
    sentinel = tmp_path / ".pre-impl"
    sentinel.touch()
    (tmp_path / "output.txt").write_text("new file")
    with patch("gremlins.stages.implement.changes_outside_git", return_value=True):
        stage.run(state)
    assert len(state.client.calls) == 1
    assert state.client.calls[0].label == "implement"


def test_local_raises_when_no_changes(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(
        tmp_path,
        is_git=False,
        prompts=[_TEMPLATE_LOCAL],
    )
    with patch("gremlins.stages.implement.changes_outside_git", return_value=False):
        with pytest.raises(RuntimeError, match="no changes"):
            stage.run(state)


def test_local_git_succeeds_on_head_advanced(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, is_git=True, prompts=[_TEMPLATE_LOCAL])
    with (
        patch(
            "gremlins.stages.implement.record_pre_impl_state", return_value=_FAKE_PRE
        ),
        patch(
            "gremlins.stages.implement.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=2),
        ),
    ):
        stage.run(state)
    assert len(state.client.calls) == 1


def test_local_git_raises_on_empty_impl(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, is_git=True, prompts=[_TEMPLATE_LOCAL])
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
        stage.run(state)


def test_gh_calls_claude_with_issue_body(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, plan_text="issue body here", prompts=[_TEMPLATE_GH], is_gh=True
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
        stage.run(state)
    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "implement"
    assert "issue body here" in call.prompt


def test_gh_plan_source_label_with_issue_num(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(json.dumps({"issue_num": "99"}), encoding="utf-8")
    monkeypatch.setattr(
        "gremlins.stages.implement.resolve_state_file", lambda gr_id=None: state_file
    )
    stage, state = _make_state(
        tmp_path, plan_text="body", prompts=[_TEMPLATE_GH], is_gh=True
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
        stage.run(state)
    prompt = state.client.calls[0].prompt
    assert "from the GitHub issue" in prompt


def test_gh_plan_source_label_without_issue_num(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, plan_text="body", prompts=[_TEMPLATE_GH], is_gh=True
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
        stage.run(state)
    prompt = state.client.calls[0].prompt
    assert "below" in prompt


def test_local_git_raises_on_divergent_head(
    tmp_path: pathlib.Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    stage, state = _make_state(tmp_path, is_git=True, prompts=[_TEMPLATE_LOCAL])
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
        stage.run(state)


def test_gh_raises_on_empty_impl(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, plan_text="body", prompts=[_TEMPLATE_GH], is_gh=True
    )
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
        stage.run(state)


def test_gh_raises_on_divergent_head(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, plan_text="body", prompts=[_TEMPLATE_GH], is_gh=True
    )
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
        stage.run(state)
