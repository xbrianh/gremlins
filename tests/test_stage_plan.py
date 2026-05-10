"""Tests for Plan stage plan resolution."""

from __future__ import annotations

import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import StageContext
from gremlins.stages.plan import Plan


def _ctx(session_dir: pathlib.Path, client: FakeClaudeClient) -> StageContext:
    return StageContext(client=client, session_dir=session_dir, gr_id=None)


def test_plan_source_file_local(tmp_path: pathlib.Path) -> None:
    """plan=<file> with no repo just copies the file to plan.md."""
    plan_src = tmp_path / "my-plan.md"
    plan_src.write_text("# My Plan\nDo stuff.\n")

    stage = Plan("plan", None, [], {}, plan=str(plan_src))
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    plan_md = tmp_path / "plan.md"
    assert plan_md.exists()
    assert plan_md.read_text() == "# My Plan\nDo stuff.\n"
    assert client.calls == []


def test_plan_source_issue_ref_local(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan=#42 with no repo fetches issue body and writes plan.md."""
    monkeypatch.setattr("gremlins.stages.plan.get_repo", lambda: "owner/repo")

    def _fake_view_issue(_ref: str, _repo: str) -> dict[str, object]:
        return {"body": "# Issue Plan\nDetails.", "url": "", "number": 42, "title": ""}

    def _fake_parse_issue_ref(_ref: str, _default: str) -> tuple[str, str]:
        return ("owner/repo", "42")

    monkeypatch.setattr("gremlins.stages.plan.view_issue", _fake_view_issue)
    monkeypatch.setattr("gremlins.stages.plan.parse_issue_ref", _fake_parse_issue_ref)

    stage = Plan("plan", None, [], {}, plan="#42")
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    plan_md = tmp_path / "plan.md"
    assert plan_md.exists()
    assert "Issue Plan" in plan_md.read_text()
    assert client.calls == []


def test_plan_source_file_github(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan=<file> with repo set creates a GitHub issue and copies the file."""
    import subprocess as _subprocess

    plan_src = tmp_path / "spec.md"
    plan_src.write_text("# Feature\nDo the thing.\n")

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
        "plan",
        None,
        [],
        {},
        plan=str(plan_src),
        repo="owner/repo",
    )
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    plan_md = tmp_path / "plan.md"
    assert plan_md.exists()
    assert plan_md.read_text() == "# Feature\nDo the thing.\n"
    assert any(c.label == "plan-title" for c in client.calls)


def test_plan_source_issue_ref_github(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan=#99 with repo set fetches issue and populates pipe."""
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

    stage = Plan("plan", None, [], {}, plan="#99", repo="owner/repo")
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    plan_md = tmp_path / "plan.md"
    assert plan_md.exists()
    assert "GH Plan" in plan_md.read_text()
    assert client.calls == []


def test_plan_reuses_existing_plan_md(tmp_path: pathlib.Path) -> None:
    """If plan.md already exists, Plan.run returns without calling the agent."""
    plan_md = tmp_path / "plan.md"
    plan_md.write_text("# Cached Plan\n")

    stage = Plan("plan", None, [], {})
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)

    assert client.calls == []
    assert plan_md.read_text() == "# Cached Plan\n"


def test_plan_without_plan_resolves_session_dir(tmp_path: pathlib.Path) -> None:
    """Constructing Plan without plan= resolves plan_md to session_dir/plan.md."""
    (tmp_path / "plan.md").write_text("# Existing\n")
    stage = Plan("plan", None, [], {})
    client = FakeClaudeClient(fixtures={})
    stage.bind(_ctx(tmp_path, client))
    stage.run(None)
    assert client.calls == []


# --- _resolve_issue_source: same-repo / cross-repo guard ---


def _issue_source_mocks(monkeypatch: pytest.MonkeyPatch, pr_repo: str = "owner/repo") -> None:
    monkeypatch.setattr("gremlins.stages.plan.get_repo", lambda: pr_repo)
    monkeypatch.setattr(
        "gremlins.stages.plan.view_issue",
        lambda _ref, _repo: {
            "body": "# Plan\nDo the thing.",
            "url": f"https://github.com/{_repo}/issues/355",
            "number": 355,
            "title": "Fix it",
        },
    )


def test_resolve_issue_source_empty_repo_writes_url(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """repo='' (gh-terse default) should write the resolved issue_url."""
    _issue_source_mocks(monkeypatch, pr_repo="owner/repo")
    captured: dict[str, object] = {}
    monkeypatch.setattr(
        "gremlins.stages.plan.patch_state",
        lambda _id, **kw: captured.update(kw),
    )
    stage = Plan("plan", None, [], {}, plan="#355", repo="")
    stage.bind(_ctx(tmp_path, FakeClaudeClient(fixtures={})))
    stage.run(None)
    assert captured.get("issue_url") == "https://github.com/owner/repo/issues/355"
    assert captured.get("issue_num") == "355"


def test_resolve_issue_source_matching_repo_writes_url(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Explicit repo matching target_repo should write the resolved issue_url."""
    _issue_source_mocks(monkeypatch, pr_repo="owner/repo")
    captured: dict[str, object] = {}
    monkeypatch.setattr(
        "gremlins.stages.plan.patch_state",
        lambda _id, **kw: captured.update(kw),
    )
    stage = Plan("plan", None, [], {}, plan="#355", repo="owner/repo")
    stage.bind(_ctx(tmp_path, FakeClaudeClient(fixtures={})))
    stage.run(None)
    assert captured.get("issue_url") == "https://github.com/owner/repo/issues/355"
    assert captured.get("issue_num") == "355"


def test_resolve_issue_source_cross_repo_clears_url(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Cross-repo ref (owner/b#355) with repo=owner/a should clear issue_url."""
    _issue_source_mocks(monkeypatch, pr_repo="owner/a")
    captured: dict[str, object] = {}
    monkeypatch.setattr(
        "gremlins.stages.plan.patch_state",
        lambda _id, **kw: captured.update(kw),
    )
    stage = Plan("plan", None, [], {}, plan="owner/b#355", repo="owner/a")
    stage.bind(_ctx(tmp_path, FakeClaudeClient(fixtures={})))
    stage.run(None)
    assert captured.get("issue_url") == ""
    assert captured.get("issue_num") == ""
