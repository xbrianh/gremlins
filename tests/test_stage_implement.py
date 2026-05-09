"""Tests for gremlins.stages.implement."""

from __future__ import annotations

import json
import pathlib
from types import SimpleNamespace
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import StageContext
from gremlins.stages.implement import Implement

_TEMPLATE_LOCAL = "plan: {plan_text}{spec_block}{impl_commit_instr}"
_TEMPLATE_GH = "{spec_block}{plan_source_label}{issue_body}{plan_location_note}"


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    plan_text: str = "do the thing",
    is_git: bool = True,
    spec_text: str = "",
    prompts: list[str] | None = None,
) -> tuple[Implement, StageContext]:
    stage = Implement("implement", "sonnet", prompts or [], {}, is_git=is_git, spec_text=spec_text)
    client = FakeClaudeClient(fixtures={"implement": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=None)
    stage.bind(ctx)
    (tmp_path / "plan.md").write_text(plan_text, encoding="utf-8")
    return stage, ctx


def test_local_calls_claude(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, ctx = _make_stage(
        tmp_path,
        is_git=False,
        prompts=[_TEMPLATE_LOCAL],
    )
    sentinel = tmp_path / ".pre-impl"
    sentinel.touch()
    (tmp_path / "output.txt").write_text("new file")
    with patch("gremlins.stages.implement.changes_outside_git", return_value=True):
        stage.run(None)
    assert len(ctx.client.calls) == 1
    assert ctx.client.calls[0].label == "implement"


def test_local_raises_when_no_changes(tmp_path: pathlib.Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    stage, ctx = _make_stage(
        tmp_path,
        is_git=False,
        prompts=[_TEMPLATE_LOCAL],
    )
    with patch("gremlins.stages.implement.changes_outside_git", return_value=False):
        with pytest.raises(RuntimeError, match="no changes"):
            stage.run(None)


def test_gh_calls_claude_with_issue_body(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(
        tmp_path, plan_text="issue body here", prompts=[_TEMPLATE_GH]
    )
    pipe = SimpleNamespace(target="github")
    stage.run(pipe)
    assert len(ctx.client.calls) == 1
    call = ctx.client.calls[0]
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
    stage, ctx = _make_stage(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    pipe = SimpleNamespace(target="github")
    stage.run(pipe)
    prompt = ctx.client.calls[0].prompt
    assert "from the GitHub issue" in prompt


def test_gh_plan_source_label_without_issue_num(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, plan_text="body", prompts=[_TEMPLATE_GH])
    pipe = SimpleNamespace(target="github")
    stage.run(pipe)
    prompt = ctx.client.calls[0].prompt
    assert "below" in prompt


def test_run_raises_if_unbound() -> None:
    stage = Implement("implement", None, [], {}, is_git=False)
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(None)
