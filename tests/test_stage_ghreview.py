"""Tests for ReviewCode.results_to_github (formerly gremlins.stages.ghreview)."""

from __future__ import annotations

import pathlib
import types

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import StageContext
from gremlins.stages.review_code import ReviewCode

PR_URL = "https://github.com/owner/repo/pull/42"
_GH_PIPE = types.SimpleNamespace(target="github")


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str = PR_URL,
    style_content: str | None = None,
) -> tuple[ReviewCode, FakeClaudeClient]:
    prompt_text = "Review PR {pr_url}."
    prompts = (
        [style_content, prompt_text] if style_content is not None else [prompt_text]
    )
    stage = ReviewCode(
        "ghreview", "sonnet", prompts, {}, plan_text="", is_git=True, pr_url=pr_url
    )
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    stage.bind(StageContext(client=client, session_dir=tmp_path, gr_id=gr_id))
    return stage, client


def test_run_calls_claude_with_pr_url(tmp_path: pathlib.Path) -> None:
    stage, client = _make_stage(tmp_path)
    stage.run(_GH_PIPE)
    assert len(client.calls) == 1
    call = client.calls[0]
    assert PR_URL in call.prompt
    assert call.label == "ghreview"


def test_run_includes_style_from_prompt_paths(tmp_path: pathlib.Path) -> None:
    stage, client = _make_stage(tmp_path, style_content="Use type hints.")
    stage.run(_GH_PIPE)
    assert "Use type hints." in client.calls[0].prompt


def test_run_raises_if_unbound() -> None:
    stage = ReviewCode(
        "ghreview",
        None,
        ["Review PR {pr_url}."],
        {},
        plan_text="",
        is_git=True,
        pr_url=PR_URL,
    )
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(_GH_PIPE)


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, client = _make_stage(tmp_path)
    stage.run(_GH_PIPE)
    call = client.calls[0]
    assert call.raw_path == tmp_path / "stream-ghreview.jsonl"
