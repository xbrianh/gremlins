"""Tests for GitHubAddressPullRequestReviews.run."""

from __future__ import annotations

import argparse
import json
import pathlib

import pytest
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
    stage.run(state)
    assert len(client.calls) == 1
    call = client.calls[0]
    assert PR_URL in call.prompt
    assert call.label == "github-address-pull-request-reviews"


def test_run_includes_style_from_prompt_paths(tmp_path: pathlib.Path) -> None:
    stage, client, state = _make_stage(tmp_path, style_content="Use type hints.")
    stage.run(state)
    assert "Use type hints." in client.calls[0].prompt


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, client, state = _make_stage(tmp_path)
    stage.run(state)
    call = client.calls[0]
    assert (
        call.raw_path == tmp_path / "stream-github-address-pull-request-reviews.jsonl"
    )


# --- PR seeding tests ---


def _make_stage_with_state_dir(
    tmp_path: pathlib.Path,
    gremlin_id: str = "test-gremlin",
    *,
    pr_url: str = "",
) -> tuple[GitHubAddressPullRequestReviews, FakeClaudeClient, RuntimeState]:
    state_dir = tmp_path / gremlin_id
    state_dir.mkdir()
    (state_dir / "state.json").write_text(
        json.dumps({"id": gremlin_id, "artifacts": []}), encoding="utf-8"
    )
    session_dir = state_dir / "artifacts"
    session_dir.mkdir()

    stage = GitHubAddressPullRequestReviews(
        "github-address-pull-request-reviews",
        ["Address PR {pr_url}."],
        {},
        pr_url=pr_url,
    )
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    data = StateData(gremlin_id=gremlin_id, state_file=state_dir / "state.json")
    state = RuntimeState(
        data=data,
        client=client,
        session_dir=session_dir,
        pipeline_data=_gh_pipeline(),
        repo="owner/name",
    )
    return stage, client, state


def test_seed_pr_artifact_by_number(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        "gremlins.utils.github.view_pr_head_branch",
        lambda url: "feature-branch",
    )
    stage, client, state = _make_stage_with_state_dir(tmp_path)
    state.args = argparse.Namespace(pr="650")

    stage.run(state)

    assert state.data.read_pr_url() == "https://github.com/owner/name/pull/650"
    assert state.data.last_pr_branch() == "feature-branch"


def test_seed_pr_artifact_by_full_url(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        "gremlins.utils.github.view_pr_head_branch",
        lambda url: "feature-branch",
    )
    stage, client, state = _make_stage_with_state_dir(tmp_path)
    state.repo = ""
    state.args = argparse.Namespace(pr="https://github.com/owner/name/pull/650")

    stage.run(state)

    assert state.data.read_pr_url() == "https://github.com/owner/name/pull/650"


def test_no_pr_arg_no_artifact_raises(tmp_path: pathlib.Path) -> None:
    stage, client, state = _make_stage_with_state_dir(tmp_path)
    # no state.args.pr, no existing artifact → falls through to RuntimeError
    with pytest.raises(RuntimeError, match="no pr_url in state.json"):
        stage.run(state)


def test_existing_artifact_not_overwritten(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    existing_url = "https://github.com/owner/name/pull/42"
    gremlin_id = "test-gremlin"
    state_dir = tmp_path / gremlin_id
    state_dir.mkdir()
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "artifacts": [
                    {"type": "pr", "url": existing_url, "branch": "old-branch"}
                ],
            }
        ),
        encoding="utf-8",
    )
    session_dir = state_dir / "artifacts"
    session_dir.mkdir()

    called = []
    monkeypatch.setattr(
        "gremlins.utils.github.view_pr_head_branch",
        lambda url: called.append(url) or "new-branch",
    )

    stage = GitHubAddressPullRequestReviews(
        "github-address-pull-request-reviews",
        ["Address PR {pr_url}."],
        {},
    )
    client = FakeClaudeClient(
        fixtures={"github-address-pull-request-reviews": MINIMAL_EVENTS}
    )
    data = StateData(gremlin_id=gremlin_id, state_file=state_dir / "state.json")
    state = RuntimeState(
        data=data,
        client=client,
        session_dir=session_dir,
        pipeline_data=_gh_pipeline(),
        repo="owner/name",
    )
    state.args = argparse.Namespace(pr="999")

    stage.run(state)

    # gh was never called — existing artifact took priority
    assert not called
    assert state.data.read_pr_url() == existing_url
