"""Tests for gremlins.stages.github_open_pull_request."""

from __future__ import annotations

import asyncio
import dataclasses
import pathlib
from unittest.mock import MagicMock, patch

from conftest import MINIMAL_EVENTS

from gremlins.artifacts.registry import MissingArtifact
from gremlins.artifacts.schemes import PrInfo
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.github_open_pull_request import GitHubOpenPullRequest

PR_URL = "https://github.com/owner/repo/pull/42"
PR_BRANCH = "issue-42-add-feature"


def _make_state(
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
    issue_url: str = "https://github.com/owner/repo/issues/42",
) -> tuple[GitHubOpenPullRequest, RuntimeState]:
    stage = GitHubOpenPullRequest("open-pr", [], {})
    client = FakeClaudeClient(fixtures={"github-open-pull-request": MINIMAL_EVENTS})
    state = build_state(
        data=StateData(gremlin_id=gremlin_id, issue_url=issue_url),
        client=client,
        session_dir=tmp_path,
    )
    return stage, state


def test_run_calls_claude_with_push_prompt(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gremlin_id="test-gr")
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
    ):
        asyncio.run(stage.run(state))
    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "github-open-pull-request"
    assert "push" in call.prompt.lower()


def test_issue_num_adds_closes_clause(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, gremlin_id="test-gr", issue_url="https://github.com/o/r/issues/42"
    )
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
    ):
        asyncio.run(stage.run(state))
    assert "Closes #42" in state.client.calls[0].prompt


def test_no_issue_url_skips_closes(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gremlin_id="test-gr", issue_url="")
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
    ):
        asyncio.run(stage.run(state))
    assert "Include 'Closes" not in state.client.calls[0].prompt


def test_run_returns_done(tmp_path: pathlib.Path) -> None:
    from gremlins.stages.outcome import Done

    stage, state = _make_state(tmp_path, gremlin_id="test-gr")
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
    ):
        result = asyncio.run(stage.run(state))
    assert result == Done()


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gremlin_id="test-gr")
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
    ):
        asyncio.run(stage.run(state))
    assert (
        state.client.calls[0].raw_path
        == tmp_path / "stream-github-open-pull-request.jsonl"
    )


def test_run_records_pr_artifact(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gremlin_id="test-gr")
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
    ):
        asyncio.run(stage.run(state))
    assert state.artifacts.resolve("pr") == Uri.parse("gh://pr/42")


# ---------------------------------------------------------------------------
# Stacked-PR: chain_state base-ref resolution
# ---------------------------------------------------------------------------


def _make_state_with_gr(
    tmp_path: pathlib.Path,
    gremlin_id: str = "test-gr",
    *,
    base_ref_name: str = "",
    issue_url: str = "",
) -> tuple[GitHubOpenPullRequest, RuntimeState]:
    stage = GitHubOpenPullRequest("open-pr", [], {})
    client = FakeClaudeClient(fixtures={"github-open-pull-request": MINIMAL_EVENTS})
    state = build_state(
        data=StateData(
            gremlin_id=gremlin_id, base_ref_name=base_ref_name, issue_url=issue_url
        ),
        client=client,
        session_dir=tmp_path,
    )
    return stage, state


def test_stacked_pr_uses_prior_pr_branch(tmp_path: pathlib.Path) -> None:
    """For child N>1, PR base is the previous child's PR branch."""
    stage, state = _make_state_with_gr(tmp_path, base_ref_name="main")
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    # Pre-bind a prior PR in the registry and mock the resolver read
    state.artifacts.bind("pr", Uri.parse("gh://pr/1"))
    mock_resolver = MagicMock()
    mock_resolver.read.return_value = PrInfo(
        url="https://github.com/o/r/pull/1",
        number=1,
        branch="gremlin/abc-child-1",
    )
    state.artifacts._resolvers["gh"] = mock_resolver

    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert prompts_seen, "run_agent should have been called"
    assert "gremlin/abc-child-1" in prompts_seen[0], (
        "PR prompt should target previous child branch, not main"
    )


def test_single_pr_without_prior_pr_branch_uses_base_ref_name(
    tmp_path: pathlib.Path,
) -> None:
    """Regression: when no prior PR branch exists, base_ref_name is used as the PR base."""
    stage, state = _make_state_with_gr(tmp_path, base_ref_name="main")
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    # No pr bound in registry — MissingArtifact will fire
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert prompts_seen, "run_agent should have been called"
    assert "main" in prompts_seen[0]


def test_first_child_uses_base_ref_name(tmp_path: pathlib.Path) -> None:
    """For child 1 (no previous artifact branch), PR base is base_ref_name."""
    stage, state = _make_state_with_gr(tmp_path, base_ref_name="main")
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert prompts_seen, "run_agent should have been called"
    assert "main" in prompts_seen[0]


def test_explicit_base_ref_used_when_no_prior_pr(tmp_path: pathlib.Path) -> None:
    """Stage-level base_ref is used when there is no prior PR artifact branch."""
    stage = GitHubOpenPullRequest("open-pr", [], {}, base_ref="feature-base")
    client = FakeClaudeClient(fixtures={"github-open-pull-request": MINIMAL_EVENTS})
    state = build_state(
        data=StateData(gremlin_id="test-gr", base_ref_name="main"),
        client=client,
        session_dir=tmp_path,
    )
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert "feature-base" in prompts_seen[0]


def test_last_pr_branch_takes_priority_over_base_ref(tmp_path: pathlib.Path) -> None:
    """Prior PR branch from registry takes priority over stage-level base_ref when stacking."""
    stage = GitHubOpenPullRequest("open-pr", [], {}, base_ref="feature-base")
    client = FakeClaudeClient(fixtures={"github-open-pull-request": MINIMAL_EVENTS})
    state = build_state(
        data=StateData(gremlin_id="test-gr", base_ref_name="main"),
        client=client,
        session_dir=tmp_path,
    )
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    state.artifacts.bind("pr", Uri.parse("gh://pr/1"))
    mock_resolver = MagicMock()
    mock_resolver.read.return_value = PrInfo(
        url="https://github.com/o/r/pull/1",
        number=1,
        branch="gremlin/child-1",
    )
    state.artifacts._resolvers["gh"] = mock_resolver

    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert "gremlin/child-1" in prompts_seen[0]
    assert "feature-base" not in prompts_seen[0]


def test_loop_iteration_gt1_adds_iter_suffix_instruction(
    tmp_path: pathlib.Path,
) -> None:
    stage, state = _make_state(
        tmp_path, gremlin_id="test-gr", issue_url="https://github.com/o/r/issues/431"
    )
    state = dataclasses.replace(
        state, data=dataclasses.replace(state.data, loop_iteration=2)
    )
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert prompts_seen
    assert "-iter2" in prompts_seen[0]


def test_loop_iteration_1_no_iter_suffix(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, gremlin_id="test-gr", issue_url="https://github.com/o/r/issues/431"
    )
    prompts_seen: list[str] = []

    async def _fake_run_agent(state, prompt, **kw):
        prompts_seen.append(prompt)
        return type("R", (), {"events": [], "text_result": ""})()

    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value=PR_BRANCH,
        ),
        patch("gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent),
    ):
        asyncio.run(stage.run(state))
    assert prompts_seen
    assert "-iter" not in prompts_seen[0]


def test_record_child_pr_appends_pr_artifact(tmp_path: pathlib.Path) -> None:
    """After opening a PR, pr is bound in the registry."""
    stage, state = _make_state_with_gr(tmp_path, base_ref_name="main")
    with (
        patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value="https://github.com/owner/repo/pull/314",
        ),
        patch(
            "gremlins.stages.github_open_pull_request._get_pr_branch",
            return_value="issue-42-some-slug",
        ),
    ):
        asyncio.run(stage.run(state))
    assert state.artifacts.resolve("pr") == Uri.parse("gh://pr/314")
