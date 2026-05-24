"""Unit tests for gremlins.stages.github_request_copilot_review."""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState, StateData, build_state
from gremlins.stages.github_request_copilot_review import GitHubRequestCopilotReview
from gremlins.utils import proc


def _make_stage(
    tmp_path, *, repo: str, pr_num: str
) -> tuple[GitHubRequestCopilotReview, RuntimeState]:
    stage = GitHubRequestCopilotReview(
        "github-request-copilot-review", [], {}, pr_num=pr_num
    )
    state = build_state(
        data=StateData(),
        client=FakeClaudeClient(fixtures={}),
        session_dir=tmp_path,
        repo=repo,
    )
    return stage, state


def test_run_calls_gh_pr_edit(tmp_path, monkeypatch):
    calls = []

    async def fake_run_async(cmd, *args, **kwargs):
        calls.append(cmd)
        result = AsyncMock()
        result.returncode = 0
        result.stdout = ""
        result.stderr = ""
        return result

    monkeypatch.setattr(proc, "run_async", fake_run_async)

    stage, state = _make_stage(tmp_path, repo="owner/repo", pr_num="42")
    asyncio.run(stage.run(state))

    assert len(calls) == 1
    cmd = calls[0]
    assert cmd[0] == "gh"
    assert "edit" in cmd
    assert "42" in cmd
    assert "--repo" in cmd
    assert "owner/repo" in cmd
    assert "copilot-pull-request-reviewer" in cmd


def test_run_raises_on_nonzero_returncode(tmp_path, monkeypatch):
    async def fake_run_async(cmd, *args, **kwargs):
        result = AsyncMock()
        result.returncode = 1
        result.stdout = ""
        result.stderr = "review not enabled"
        return result

    monkeypatch.setattr(proc, "run_async", fake_run_async)

    stage, state = _make_stage(tmp_path, repo="owner/repo", pr_num="7")
    with pytest.raises(RuntimeError, match="could not request Copilot review"):
        asyncio.run(stage.run(state))
