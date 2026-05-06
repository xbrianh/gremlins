"""Tests for gremlins.stages.ghreview."""

from __future__ import annotations

import pathlib

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.context import StageContext
from gremlins.stages.ghreview import GHReview

PR_URL = "https://github.com/owner/repo/pull/42"
CODE_STYLE = "Be good."


def _make_entry(prompt_path: pathlib.Path) -> StageEntry:
    return StageEntry(
        name="ghreview",
        type="ghreview",
        client=None,
        prompt_paths=[prompt_path],
        options={},
    )


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str = PR_URL,
    code_style: str = CODE_STYLE,
) -> tuple[GHReview, StageContext]:
    prompt_path = tmp_path / "ghreview.md"
    prompt_path.write_text(
        "Review PR {pr_url} with style {code_style}.{bail_section}", encoding="utf-8"
    )
    entry = _make_entry(prompt_path)
    stage = GHReview(entry, "sonnet", pr_url=pr_url, code_style=code_style)
    client = FakeClaudeClient(fixtures={"ghreview": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)
    stage.bind(ctx)
    return stage, ctx


def test_run_calls_claude_with_pr_url(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    stage.run(None)
    assert len(ctx.client.calls) == 1
    call = ctx.client.calls[0]
    assert PR_URL in call.prompt
    assert call.label == "ghreview"


def test_run_includes_code_style(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, code_style="Use type hints.")
    stage.run(None)
    assert "Use type hints." in ctx.client.calls[0].prompt


def test_run_no_bail_section_without_gr_id(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, gr_id=None)
    stage.run(None)
    assert "bail marker" not in ctx.client.calls[0].prompt


def test_run_bail_section_with_gr_id(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, gr_id="test-gr")
    stage.run(None)
    assert "bail marker" in ctx.client.calls[0].prompt


def test_run_raises_if_unbound() -> None:
    prompt_path = pathlib.Path("/tmp/fake.md")
    entry = StageEntry(
        name="ghreview",
        type="ghreview",
        client=None,
        prompt_paths=[prompt_path],
        options={},
    )
    stage = GHReview(entry, None, pr_url=PR_URL, code_style=CODE_STYLE)
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(None)


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    stage.run(None)
    call = ctx.client.calls[0]
    assert call.raw_path == tmp_path / "stream-ghreview.jsonl"
