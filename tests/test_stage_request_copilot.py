"""Unit tests for gremlins.stages.request_copilot."""

import subprocess

import pytest

from gremlins.stages.context import StageContext
from gremlins.stages.request_copilot import RequestCopilotOptions, run


def _make_ctx(tmp_path):
    from gremlins.clients.fake import FakeClaudeClient

    return StageContext(
        client=FakeClaudeClient(fixtures={}),
        session_dir=tmp_path,
        gr_id=None,
    )


def test_run_calls_gh_pr_edit(tmp_path, monkeypatch):
    calls = []

    def fake_run(cmd, *args, **kwargs):
        calls.append(cmd)
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_run)

    run(_make_ctx(tmp_path), RequestCopilotOptions(repo="owner/repo", pr_num="42"))

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

    with pytest.raises(RuntimeError, match="could not request Copilot review"):
        run(_make_ctx(tmp_path), RequestCopilotOptions(repo="owner/repo", pr_num="7"))
