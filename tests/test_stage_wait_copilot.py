"""Tests for gremlins.stages.wait_copilot."""

from __future__ import annotations

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.wait_copilot import WaitCopilot


def _make_entry() -> StageEntry:
    return StageEntry(
        name="wait-copilot",
        type="wait-copilot",
        client=None,
        prompts=[],
        options={},
    )


def _make_stage(
    tmp_path,
    *,
    repo: str = "owner/repo",
    pr_num: str = "42",
    timeout: int = 600,
    interval: int = 0,
    review_checker=None,
    gr_id=None,
) -> WaitCopilot:
    entry = _make_entry()
    stage = WaitCopilot(
        entry,
        None,
        repo=repo,
        pr_num=pr_num,
        timeout=timeout,
        interval=interval,
        review_checker=review_checker,
    )
    ctx = StageContext(
        client=FakeClaudeClient(fixtures={}),
        session_dir=tmp_path,
        gr_id=gr_id,
    )
    stage.bind(ctx)
    return stage


def test_returns_review_state_immediately(tmp_path):
    stage = _make_stage(tmp_path, review_checker=lambda: "APPROVED")
    result = stage.run(None)
    assert result == "APPROVED"


def test_polls_until_review_arrives(tmp_path):
    call_count = [0]

    def checker():
        call_count[0] += 1
        return "CHANGES_REQUESTED" if call_count[0] >= 3 else None

    stage = _make_stage(tmp_path, review_checker=checker)
    result = stage.run(None)
    assert result == "CHANGES_REQUESTED"
    assert call_count[0] == 3


def test_timeout_raises(tmp_path):
    stage = _make_stage(tmp_path, timeout=0, review_checker=lambda: None)
    with pytest.raises(RuntimeError, match="timed out"):
        stage.run(None)


def test_no_pr_num_raises(tmp_path):
    stage = _make_stage(tmp_path, pr_num="")
    with pytest.raises(RuntimeError, match="no pr_url in state.json"):
        stage.run(None)
