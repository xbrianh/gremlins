"""Tests for AddressCode.results_to_github (formerly gremlins.stages.ghaddress)."""

from __future__ import annotations

import pathlib
import types

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.address_code import AddressCode
from gremlins.stages.base import StageContext

PR_URL = "https://github.com/owner/repo/pull/99"
_GH_PIPE = types.SimpleNamespace(target="github")


def _make_entry(prompt_text: str) -> StageEntry:
    return StageEntry(
        name="ghaddress",
        type="ghaddress",
        client=None,
        prompts=[prompt_text],
        options={},
    )


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    pr_url: str = PR_URL,
    style_content: str | None = None,
) -> tuple[AddressCode, FakeClaudeClient]:
    prompt_text = "Address PR {pr_url}."
    if style_content is not None:
        entry = StageEntry(
            name="ghaddress",
            type="ghaddress",
            client=None,
            prompts=[style_content, prompt_text],
            options={},
        )
    else:
        entry = _make_entry(prompt_text)
    stage = AddressCode(entry, "sonnet", is_git=True, pr_url=pr_url)
    client = FakeClaudeClient(fixtures={"ghaddress": MINIMAL_EVENTS})
    stage.bind(StageContext(client=client, session_dir=tmp_path, gr_id=gr_id))
    return stage, client


def test_run_calls_claude_with_pr_url(tmp_path: pathlib.Path) -> None:
    stage, client = _make_stage(tmp_path)
    stage.run(_GH_PIPE)
    assert len(client.calls) == 1
    call = client.calls[0]
    assert PR_URL in call.prompt
    assert call.label == "ghaddress"


def test_run_includes_style_from_prompt_paths(tmp_path: pathlib.Path) -> None:
    stage, client = _make_stage(tmp_path, style_content="Use type hints.")
    stage.run(_GH_PIPE)
    assert "Use type hints." in client.calls[0].prompt


def test_run_raises_if_unbound() -> None:
    entry = StageEntry(
        name="ghaddress",
        type="ghaddress",
        client=None,
        prompts=["Address PR {pr_url}."],
        options={},
    )
    stage = AddressCode(entry, None, is_git=True, pr_url=PR_URL)
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(_GH_PIPE)


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, client = _make_stage(tmp_path)
    stage.run(_GH_PIPE)
    call = client.calls[0]
    assert call.raw_path == tmp_path / "stream-ghaddress.jsonl"
