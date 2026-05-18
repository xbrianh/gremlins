"""Tests for GitHubAddressPullRequestReviews.run."""

from __future__ import annotations

import asyncio
import pathlib

from conftest import MINIMAL_EVENTS
from conftest import gh_pipeline as _gh_pipeline

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.address_code import GitHubAddressPullRequestReviews

PR_URL = "https://github.com/owner/repo/pull/99"


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
    pr_url: str = PR_URL,
    style_content: str | None = None,
) -> tuple[GitHubAddressPullRequestReviews, FakeClaudeClient, RuntimeState]:
    prompt_text = "Address PR {pr_url}."
    prompts = (
        [style_content, prompt_text] if style_content is not None else [prompt_text]
    )
    stage = GitHubAddressPullRequestReviews(
        "github-address-pull-request-reviews", prompts, {}, pr_url=pr_url
    )
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    state = RuntimeState(
        data=StateData(gremlin_id=gremlin_id),
        client=client,
        session_dir=tmp_path,
        pipeline_data=_gh_pipeline(),
    )
    return stage, client, state


def test_run_calls_claude_with_pr_url(tmp_path: pathlib.Path) -> None:
    stage, client, state = _make_stage(tmp_path)
    asyncio.run(stage.run(state))
    assert len(client.calls) == 1
    call = client.calls[0]
    assert PR_URL in call.prompt
    assert call.label == "github-address-pull-request-reviews"


def test_run_includes_style_from_prompt_paths(tmp_path: pathlib.Path) -> None:
    stage, client, state = _make_stage(tmp_path, style_content="Use type hints.")
    asyncio.run(stage.run(state))
    assert "Use type hints." in client.calls[0].prompt


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, client, state = _make_stage(tmp_path)
    asyncio.run(stage.run(state))
    call = client.calls[0]
    assert (
        call.raw_path == tmp_path / "stream-github-address-pull-request-reviews.jsonl"
    )
