"""Unit tests for gremlins.stages.github_request_copilot_review."""

from __future__ import annotations

import subprocess

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.stages.github_request_copilot_review import GitHubRequestCopilotReview


def _make_stage(
    tmp_path, *, repo: str, pr_num: str
) -> tuple[GitHubRequestCopilotReview, RuntimeState]:
    stage = GitHubRequestCopilotReview("request-copilot", None, [], {}, pr_num=pr_num)
    state = RuntimeState(
        client=FakeClaudeClient(fixtures={}),
        session_dir=tmp_path,
        gr_id=None,
        repo=repo,
    )
    return stage, state


def test_run_calls_gh_pr_edit(tmp_path, monkeypatch):
    calls = []

    def fake_run(cmd, *args, **kwargs):
        calls.append(cmd)
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_run)

    stage, state = _make_stage(tmp_path, repo="owner/repo", pr_num="42")
    stage.run(state)

    assert len(calls) == 1
    cmd = calls[0]
    assert cmd[0] == "gh"
    assert "edit" in cmd
    assert "42" in cmd
    assert "--repo" in cmd
    assert "owner/repo" in cmd
    assert "copilot-pull-request-reviewer" in cmd


def test_run_raises_on_nonzero_returncode(tmp_path, monkeypatch):
    def fake_run(cmd, *args, **kwargs):
        return subprocess.CompletedProcess(
            cmd, 1, stdout="", stderr="review not enabled"
        )

    monkeypatch.setattr(subprocess, "run", fake_run)

    stage, state = _make_stage(tmp_path, repo="owner/repo", pr_num="7")
    with pytest.raises(RuntimeError, match="could not request Copilot review"):
        stage.run(state)
