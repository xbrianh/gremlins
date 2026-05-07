"""Tests for Plan stage plan_source resolution."""

from __future__ import annotations

import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.plan import Plan


def _entry(prompt: str | None = None) -> StageEntry:
    return StageEntry(
        name="plan",
        type="plan",
        prompt_paths=[pathlib.Path(prompt)] if prompt else [],
        options={},
        client=None,
    )


def _ctx(session_dir: pathlib.Path, client: FakeClaudeClient) -> StageContext:
    return StageContext(client=client, session_dir=session_dir, gr_id=None)


class _SimplePipe:
    issue_url: str = ""
    issue_num: str = ""
    issue_body: str = ""


def test_plan_source_file_local(tmp_path: pathlib.Path) -> None:
    """plan_source=<file> with no repo just copies the file to plan.md."""
    plan_src = tmp_path / "my-plan.md"
    plan_src.write_text("# My Plan\nDo stuff.\n")

    plan_md = tmp_path / "plan.md"
    stage = Plan(_entry(), None, plan_source=str(plan_src), plan_file=plan_md)
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    assert plan_md.exists()
    assert plan_md.read_text() == "# My Plan\nDo stuff.\n"
    assert client.calls == []


def test_plan_source_issue_ref_local(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan_source=#42 with no repo fetches issue body and writes plan.md."""
    plan_md = tmp_path / "plan.md"

    monkeypatch.setattr("gremlins.stages.plan.get_repo", lambda: "owner/repo")

    def _fake_view_issue(_ref: str, _repo: str) -> dict[str, object]:
        return {"body": "# Issue Plan\nDetails.", "url": "", "number": 42, "title": ""}

    def _fake_parse_issue_ref(_ref: str, _default: str) -> tuple[str, str]:
        return ("owner/repo", "42")

    monkeypatch.setattr("gremlins.stages.plan.view_issue", _fake_view_issue)
    monkeypatch.setattr("gremlins.stages.plan.parse_issue_ref", _fake_parse_issue_ref)

    stage = Plan(_entry(), None, plan_source="#42", plan_file=plan_md)
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    assert plan_md.exists()
    assert "Issue Plan" in plan_md.read_text()
    assert client.calls == []


def test_plan_source_file_github(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan_source=<file> with repo set creates a GitHub issue and copies the file."""
    import subprocess as _subprocess

    plan_src = tmp_path / "spec.md"
    plan_src.write_text("# Feature\nDo the thing.\n")
    plan_md = tmp_path / "plan.md"

    issue_url = "https://github.com/owner/repo/issues/7"

    def fake_run(
        cmd: list[str], *_args: object, **_kwargs: object
    ) -> _subprocess.CompletedProcess[str]:
        if cmd[0] == "gh" and "create" in cmd:
            return _subprocess.CompletedProcess(
                cmd, 0, stdout=issue_url + "\n", stderr=""
            )
        return _subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr("gremlins.stages.plan.subprocess.run", fake_run)

    fixtures: dict[str, object] = {
        "plan-title": [
            {"type": "system", "subtype": "init"},
            {"type": "result", "subtype": "success", "result": "Feature: Do the thing"},
        ]
    }
    client = FakeClaudeClient(fixtures=fixtures)

    stage = Plan(
        _entry(), None, plan_source=str(plan_src), plan_file=plan_md, repo="owner/repo"
    )
    stage.bind(_ctx(tmp_path, client))

    pipe = _SimplePipe()
    stage.run(pipe)

    assert plan_md.exists()
    assert plan_md.read_text() == "# Feature\nDo the thing.\n"
    assert pipe.issue_url == issue_url
    assert pipe.issue_num == "7"
    assert pipe.issue_body == "# Feature\nDo the thing.\n"
    assert any(c.label == "plan-title" for c in client.calls)


def test_plan_source_issue_ref_github(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan_source=#99 with repo set fetches issue and populates pipe."""
    plan_md = tmp_path / "plan.md"
    issue_url = "https://github.com/owner/repo/issues/99"

    def _fake_parse_issue_ref(_ref: str, _default: str) -> tuple[str, str]:
        return ("owner/repo", "99")

    def _fake_view_issue(_ref: str, _repo: str) -> dict[str, object]:
        return {
            "body": "# GH Plan\nDetails.",
            "url": issue_url,
            "number": 99,
            "title": "GH Plan",
        }

    monkeypatch.setattr("gremlins.stages.plan.parse_issue_ref", _fake_parse_issue_ref)
    monkeypatch.setattr("gremlins.stages.plan.view_issue", _fake_view_issue)

    stage = Plan(
        _entry(), None, plan_source="#99", plan_file=plan_md, repo="owner/repo"
    )
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))

    pipe = _SimplePipe()
    stage.run(pipe)

    assert plan_md.exists()
    assert "GH Plan" in plan_md.read_text()
    assert pipe.issue_url == issue_url
    assert pipe.issue_num == "99"
    assert pipe.issue_body == "# GH Plan\nDetails."
    assert client.calls == []


def test_plan_reuses_existing_plan_md(tmp_path: pathlib.Path) -> None:
    """If plan.md already exists, Plan.run returns without calling the agent."""
    plan_md = tmp_path / "plan.md"
    plan_md.write_text("# Cached Plan\n")

    stage = Plan(_entry(), None, plan_file=plan_md)
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    assert client.calls == []
    assert plan_md.read_text() == "# Cached Plan\n"
