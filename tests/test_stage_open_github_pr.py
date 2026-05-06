"""Tests for gremlins.stages.open_github_pr."""

from __future__ import annotations

import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.open_github_pr import OpenGitHubPR

ISSUE_URL = "https://github.com/owner/repo/issues/42"
PR_URL = "https://github.com/owner/repo/pull/42"


def _make_entry() -> StageEntry:
    return StageEntry(
        name="open-pr",
        type="open-github-pr",
        client=None,
        prompt_paths=[],
        options={},
    )


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    issue_url: str = ISSUE_URL,
) -> tuple[OpenGitHubPR, StageContext]:
    entry = _make_entry()
    stage = OpenGitHubPR(entry, "sonnet", issue_url=issue_url)
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=None)
    stage.bind(ctx)
    return stage, ctx


def test_run_calls_claude_with_push_prompt(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert len(ctx.client.calls) == 1
    call = ctx.client.calls[0]
    assert call.label == "open-github-pr"
    assert "push" in call.prompt.lower()


def test_issue_num_adds_closes_clause(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, issue_url="https://github.com/o/r/issues/42")
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert "Closes #42" in ctx.client.calls[0].prompt


def test_no_issue_url_skips_closes(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, issue_url="")
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert "Include 'Closes" not in ctx.client.calls[0].prompt


def test_run_returns_pr_url(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        result = stage.run(None)
    assert result == PR_URL


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert ctx.client.calls[0].raw_path == tmp_path / "stream-open-github-pr.jsonl"


def test_run_raises_if_unbound() -> None:
    entry = _make_entry()
    stage = OpenGitHubPR(entry, None, issue_url=ISSUE_URL)
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(None)
