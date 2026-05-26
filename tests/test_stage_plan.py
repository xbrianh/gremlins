"""Tests for Plan stage plan resolution."""

from __future__ import annotations

import argparse
import asyncio
import pathlib

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.plan import Plan


def _state(session_dir: pathlib.Path, client: FakeClaudeClient, *, repo: str = "") -> RuntimeState:
    return build_state(data=StateData(), client=client, session_dir=session_dir, repo=repo)


def _state_with_artifacts(
    session_dir: pathlib.Path, client: FakeClaudeClient, *, repo: str = ""
) -> RuntimeState:
    # Ensure registry_path (derived from session_dir.parent) lands in an
    # isolated per-test directory: caller passes tmp_path, we use a subdir
    # so tmp_path acts as the "state_dir" parent.
    sd = session_dir / "session"
    sd.mkdir(parents=True, exist_ok=True)
    return build_state(data=StateData(), client=client, session_dir=sd, repo=repo)


class _PlanWritingClient(FakeClaudeClient):
    """Writes plan.md to session_dir when the 'plan' agent runs."""

    def __init__(self, session_dir: pathlib.Path) -> None:
        super().__init__(fixtures={"plan": MINIMAL_EVENTS})
        self._session_dir = session_dir

    async def run(self, prompt: str, *, label: str, **kwargs):  # type: ignore[override]
        if label == "plan":
            (self._session_dir / "plan.md").write_text("# Plan\n", encoding="utf-8")
        return await super().run(prompt, label=label, **kwargs)


def test_plan_source_file_local(tmp_path: pathlib.Path) -> None:
    """plan=<file> with no repo just copies the file to plan.md."""
    plan_src = tmp_path / "my-plan.md"
    plan_src.write_text("# My Plan\nDo stuff.\n")

    stage = Plan("plan", [], {})
    client = FakeClaudeClient(fixtures={})
    state = _state(tmp_path, client)
    state.args = argparse.Namespace(plan=str(plan_src))
    asyncio.run(stage.run(state))

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

    stage = Plan("plan", [], {})
    client = FakeClaudeClient(fixtures={})
    state = _state(tmp_path, client)
    state.args = argparse.Namespace(plan="#42")
    asyncio.run(stage.run(state))

    plan_md = tmp_path / "plan.md"
    assert plan_md.exists()
    assert "Issue Plan" in plan_md.read_text()
    assert client.calls == []


def test_plan_source_file_github(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """plan=<file> with repo set creates a GitHub issue and copies the file."""
    from gremlins.utils import proc

    plan_src = tmp_path / "spec.md"
    plan_src.write_text("# Feature\nDo the thing.\n")

    issue_url = "https://github.com/owner/repo/issues/7"

    async def fake_run_async(
        cmd: list[str], *_args: object, **_kwargs: object
    ) -> object:
        from unittest.mock import AsyncMock

        r = AsyncMock()
        if cmd[0] == "gh" and "create" in cmd:
            r.returncode = 0
            r.stdout = issue_url + "\n"
            r.stderr = ""
        else:
            r.returncode = 0
            r.stdout = ""
            r.stderr = ""
        return r

    monkeypatch.setattr(proc, "run_async", fake_run_async)

    fixtures: dict[str, object] = {
        "plan-title": [
            {"type": "system", "subtype": "init"},
            {"type": "result", "subtype": "success", "result": "Feature: Do the thing"},
        ]
    }
    client = FakeClaudeClient(fixtures=fixtures)

    stage = Plan("plan", [], {})
    state = _state(tmp_path, client, repo="owner/repo")
    state.args = argparse.Namespace(plan=str(plan_src))
    asyncio.run(stage.run(state))

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

    stage = Plan("plan", [], {})
    client = FakeClaudeClient(fixtures={})
    state = _state(tmp_path, client, repo="owner/repo")
    state.args = argparse.Namespace(plan="#99")
    asyncio.run(stage.run(state))

    plan_md = tmp_path / "plan.md"
    assert plan_md.exists()
    assert "GH Plan" in plan_md.read_text()
    assert client.calls == []


def test_plan_reuses_existing_plan_md(tmp_path: pathlib.Path) -> None:
    """If plan.md already exists, Plan.run returns without calling the agent."""
    plan_md = tmp_path / "plan.md"
    plan_md.write_text("# Cached Plan\n")

    stage = Plan("plan", ["dummy prompt"], {})
    client = FakeClaudeClient(fixtures={})
    asyncio.run(stage.run(_state(tmp_path, client)))

    assert client.calls == []
    assert plan_md.read_text() == "# Cached Plan\n"


def test_plan_without_plan_resolves_session_dir(tmp_path: pathlib.Path) -> None:
    """Constructing Plan without plan= resolves plan_md to session_dir/plan.md."""
    (tmp_path / "plan.md").write_text("# Existing\n")
    stage = Plan("plan", ["dummy prompt"], {})
    client = FakeClaudeClient(fixtures={})
    asyncio.run(stage.run(_state(tmp_path, client)))
    assert client.calls == []


# --- _resolve_issue_source: same-repo / cross-repo guard ---


def _issue_source_mocks(
    monkeypatch: pytest.MonkeyPatch, pr_repo: str = "owner/repo"
) -> None:
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
    """repo='' (gh-terse default) should bind the resolved issue in the registry."""
    _issue_source_mocks(monkeypatch, pr_repo="owner/repo")
    stage = Plan("plan", [], {})
    client = FakeClaudeClient(fixtures={})
    state = _state_with_artifacts(tmp_path, client)
    state.args = argparse.Namespace(plan="#355")
    asyncio.run(stage.run(state))
    assert state.artifacts.produced("plan")
    assert state.artifacts.resolve("plan") == Uri.parse("gh://issue/355")


def test_resolve_issue_source_matching_repo_writes_url(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Explicit repo matching target_repo should bind the resolved issue in the registry."""
    _issue_source_mocks(monkeypatch, pr_repo="owner/repo")
    stage = Plan("plan", [], {})
    client = FakeClaudeClient(fixtures={})
    state = _state_with_artifacts(tmp_path, client, repo="owner/repo")
    state.args = argparse.Namespace(plan="#355")
    asyncio.run(stage.run(state))
    assert state.artifacts.produced("plan")
    assert state.artifacts.resolve("plan") == Uri.parse("gh://issue/355")


def test_resolve_issue_source_cross_repo_clears_url(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Cross-repo ref (owner/b#355) with repo=owner/a should not bind issue in the registry."""
    _issue_source_mocks(monkeypatch, pr_repo="owner/a")
    stage = Plan("plan", [], {})
    client = FakeClaudeClient(fixtures={})
    state = _state_with_artifacts(tmp_path, client, repo="owner/a")
    state.args = argparse.Namespace(plan="owner/b#355")
    asyncio.run(stage.run(state))
    assert state.artifacts.resolve("plan") == Uri.parse("file://session/plan.md")


# --- Agent delegation (local branch) ---


def test_plan_agent_local_delegates_via_agent(tmp_path: pathlib.Path) -> None:
    """Local agent branch calls run_agent via Agent and plan.md passes verify_produced."""
    stage = Plan("plan", ["write a plan to {plan_file}"], {})
    state = _state_with_artifacts(tmp_path, FakeClaudeClient(fixtures={}))
    client = _PlanWritingClient(state.session_dir)
    state.client = client
    asyncio.run(stage.run(state))
    assert any(c.label == "plan" for c in client.calls)
    assert (state.session_dir / "plan.md").read_text() == "# Plan\n"


def test_plan_agent_local_verify_catches_missing_plan(tmp_path: pathlib.Path) -> None:
    """verify_produced raises FileNotFoundError when agent doesn't write plan.md."""
    stage = Plan("plan", ["write a plan to {plan_file}"], {})
    client = FakeClaudeClient(fixtures={"plan": MINIMAL_EVENTS})
    state = _state_with_artifacts(tmp_path, client)
    with pytest.raises(FileNotFoundError, match="plan.md"):
        asyncio.run(stage.run(state))
