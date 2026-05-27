"""Tests for GitHubOpenPullRequest."""

from __future__ import annotations

import asyncio
import dataclasses
import pathlib
from unittest.mock import patch

from gremlins.artifacts.engine import EngineContext
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.schemes import PrInfo
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import StateData, build_state
from gremlins.stages.github_open_pull_request import GitHubOpenPullRequest
from gremlins.stages.outcome import Done

PR_URL = "https://github.com/owner/repo/pull/42"


def _state(
    tmp_path: pathlib.Path,
    *,
    plan_issue: int | None = None,
    prev_pr_branch: str | None = None,
    base_ref: str = "",
    loop_iteration: int = 1,
):
    session_dir = tmp_path / "artifacts"
    session_dir.mkdir(exist_ok=True)
    registry = ArtifactRegistry(session_dir)
    if plan_issue is not None:
        registry.bind("plan", Uri.parse(f"gh://issue/{plan_issue}"))
    if prev_pr_branch is not None:
        registry.bind("pr", Uri.parse("gh://pr/1"))
        registry._resolvers["gh"].read = (  # type: ignore[attr-defined]
            lambda uri, _b=prev_pr_branch: PrInfo(
                url="https://github.com/x/r/pull/1", number=1, branch=_b
            )
        )
    engine_ctx = EngineContext(
        loop_iteration=loop_iteration, attempt="", current_scope=(), base_ref=base_ref
    )
    return build_state(
        data=dataclasses.replace(StateData(), loop_iteration=loop_iteration),
        client=None,
        session_dir=session_dir,
        artifacts=registry,
        engine_ctx=engine_ctx,
    )


def _fake_run_agent(prompts: list[str]):
    async def _run(state, prompt, **kw):
        prompts.append(prompt)
        return type("R", (), {"events": [], "text_result": PR_URL})()

    return _run


def test_prompt_includes_explicit_push_and_pr_create(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(stage.run(_state(tmp_path)))
    assert "git push origin" in prompts[0]
    assert "gh pr create" in prompts[0]
    assert "Do NOT use" in prompts[0] and "--fill" in prompts[0]


def test_closes_clause_injected_when_plan_is_gh_issue(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(stage.run(_state(tmp_path, plan_issue=99)))
    assert "Closes #99" in prompts[0]


def test_no_closes_clause_when_no_plan(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(stage.run(_state(tmp_path)))
    assert "Include 'Closes" not in prompts[0]


def test_iter_suffix_added_when_loop_iteration_gt1(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(stage.run(_state(tmp_path, loop_iteration=2)))
    assert "-iter2" in prompts[0]


def test_no_iter_suffix_on_first_iteration(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(stage.run(_state(tmp_path, loop_iteration=1)))
    assert "-iter" not in prompts[0]


def test_stacked_pr_uses_prev_branch_as_base(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(
                stage.run(
                    _state(tmp_path, prev_pr_branch="feat-child-1", base_ref="main")
                )
            )
    assert "feat-child-1" in prompts[0]
    assert (
        "main" not in prompts[0].split("--base")[1] if "--base" in prompts[0] else True
    )


def test_base_ref_used_when_no_prior_pr(tmp_path):
    prompts: list[str] = []
    stage = GitHubOpenPullRequest("open-pr", [], {})
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent(prompts)
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            asyncio.run(stage.run(_state(tmp_path, base_ref="develop")))
    assert "develop" in prompts[0]


def test_pr_artifact_bound_after_run(tmp_path):
    stage = GitHubOpenPullRequest("open-pr", [], {})
    state = _state(tmp_path)
    with patch(
        "gremlins.stages.github_open_pull_request.run_agent", _fake_run_agent([])
    ):
        with patch(
            "gremlins.stages.github_open_pull_request.extract_gh_url",
            return_value=PR_URL,
        ):
            result = asyncio.run(stage.run(state))
    assert result == Done()
    assert state.artifacts.produced("pr")
    assert state.artifacts.resolve("pr") == Uri.parse("gh://pr/42")
