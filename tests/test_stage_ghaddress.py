"""Tests for gremlins.stages.ghaddress."""

from __future__ import annotations

import pathlib

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.ghaddress import GHAddress

PR_URL = "https://github.com/owner/repo/pull/99"


def _make_entry(prompt_path: pathlib.Path) -> StageEntry:
    return StageEntry(
        name="ghaddress",
        type="ghaddress",
        client=None,
        prompt_paths=[prompt_path],
        options={},
    )


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str = PR_URL,
    style_content: str | None = None,
) -> tuple[GHAddress, StageContext]:
    prompt_path = tmp_path / "ghaddress.md"
    prompt_path.write_text("Address PR {pr_url}.", encoding="utf-8")
    if style_content is not None:
        style_path = tmp_path / "style.md"
        style_path.write_text(style_content, encoding="utf-8")
        entry = StageEntry(
            name="ghaddress",
            type="ghaddress",
            client=None,
            prompt_paths=[style_path, prompt_path],
            options={},
        )
    else:
        entry = _make_entry(prompt_path)
    stage = GHAddress(entry, "sonnet", pr_url=pr_url)
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)
    stage.bind(ctx)
    return stage, ctx


def test_run_calls_claude_with_pr_url(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    stage.run(None)
    assert len(ctx.client.calls) == 1
    call = ctx.client.calls[0]
    assert PR_URL in call.prompt
    assert call.label == "ghaddress"


def test_run_includes_style_from_prompt_paths(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, style_content="Use type hints.")
    stage.run(None)
    assert "Use type hints." in ctx.client.calls[0].prompt


def test_run_raises_if_unbound() -> None:
    prompt_path = (
        pathlib.Path(__file__).resolve().parent.parent
        / "gremlins"
        / "prompts"
        / "address_gh.md"
    )
    entry = StageEntry(
        name="ghaddress",
        type="ghaddress",
        client=None,
        prompt_paths=[prompt_path],
        options={},
    )
    stage = GHAddress(entry, None, pr_url=PR_URL)
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(None)


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    stage.run(None)
    call = ctx.client.calls[0]
    assert call.raw_path == tmp_path / "stream-ghaddress.jsonl"
