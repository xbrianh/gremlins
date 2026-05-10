"""Tests for gremlins.stages.open_github_pr."""

from __future__ import annotations

import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import RuntimeState
from gremlins.stages.open_github_pr import OpenGitHubPR

PR_URL = "https://github.com/owner/repo/pull/42"


def _make_state(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    issue_url: str = "https://github.com/owner/repo/issues/42",
) -> tuple[OpenGitHubPR, RuntimeState]:
    stage = OpenGitHubPR("open-pr", "sonnet", [], {})
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id=gr_id,
        issue_url=issue_url,
        impl_materialized_branch="impl/feature-abc123",
    )
    return stage, state


def test_run_calls_claude_with_push_prompt(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gr_id="test-gr")
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
    ):
        stage.run(state)
    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "open-github-pr"
    assert "push" in call.prompt.lower()


def test_issue_num_adds_closes_clause(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(
        tmp_path, gr_id="test-gr", issue_url="https://github.com/o/r/issues/42"
    )
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
    ):
        stage.run(state)
    assert "Closes #42" in state.client.calls[0].prompt


def test_no_issue_url_skips_closes(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gr_id="test-gr", issue_url="")
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
    ):
        stage.run(state)
    assert "Include 'Closes" not in state.client.calls[0].prompt


def test_run_returns_pr_url(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gr_id="test-gr")
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
    ):
        result = stage.run(state)
    assert result == PR_URL


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gr_id="test-gr")
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
    ):
        stage.run(state)
    assert state.client.calls[0].raw_path == tmp_path / "stream-open-github-pr.jsonl"


def test_run_records_pr_artifact(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state(tmp_path, gr_id="test-gr")
    artifact_calls: list[tuple] = []
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch(
            "gremlins.stages.open_github_pr.append_artifact",
            side_effect=lambda gr_id, artifact: artifact_calls.append(
                (gr_id, artifact)
            ),
        ),
    ):
        stage.run(state)
    assert len(artifact_calls) == 1
    assert artifact_calls[0][1]["type"] == "pr"
    assert artifact_calls[0][1]["url"] == PR_URL


# ---------------------------------------------------------------------------
# Stacked-PR: chain_state base-ref resolution
# ---------------------------------------------------------------------------


def _make_state_with_gr(
    tmp_path: pathlib.Path,
    gr_id: str = "test-gr",
    *,
    base_ref_name: str = "",
    impl_materialized_branch: str = "",
    issue_url: str = "",
) -> tuple[OpenGitHubPR, RuntimeState]:
    stage = OpenGitHubPR("open-pr", "sonnet", [], {})
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id=gr_id,
        base_ref_name=base_ref_name,
        impl_materialized_branch=impl_materialized_branch,
        issue_url=issue_url,
    )
    return stage, state


def test_stacked_pr_uses_prior_pr_branch(tmp_path: pathlib.Path) -> None:
    """For child N>1, PR base is the previous child's PR branch."""
    stage, state = _make_state_with_gr(
        tmp_path,
        base_ref_name="main",
        impl_materialized_branch="gremlin/child-2",
    )
    prompts_seen: list[str] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.last_pr_branch",
            return_value="gremlin/abc-child-1",
        ),
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
        patch.object(
            stage,
            "run_claude",
            side_effect=lambda prompt, **kw: (
                prompts_seen.append(prompt)
                or type("R", (), {"events": [], "text_result": ""})()
            ),
        ),
    ):
        stage.run(state)
    assert prompts_seen, "run_claude should have been called"
    assert "gremlin/abc-child-1" in prompts_seen[0], (
        "PR prompt should target previous child branch, not main"
    )


def test_single_pr_with_branch_artifact_uses_base_ref_name(
    tmp_path: pathlib.Path,
) -> None:
    """Regression: a branch artifact (just materialized) must NOT be used as PR base."""
    stage, state = _make_state_with_gr(
        tmp_path,
        base_ref_name="main",
        impl_materialized_branch="ghgremlin-impl-foo-abc123",
    )
    prompts_seen: list[str] = []
    with (
        patch("gremlins.state.resolve_state_file", return_value=tmp_path / "state.json"),
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
        patch.object(
            stage,
            "run_claude",
            side_effect=lambda prompt, **kw: (
                prompts_seen.append(prompt)
                or type("R", (), {"events": [], "text_result": ""})()
            ),
        ),
    ):
        stage.run(state)
    assert prompts_seen, "run_claude should have been called"
    assert "main" in prompts_seen[0]
    assert "ghgremlin-impl-foo-abc123" not in prompts_seen[0], (
        "the just-materialized head branch must not appear as the PR base"
    )


def test_first_child_uses_base_ref_name(tmp_path: pathlib.Path) -> None:
    """For child 1 (no previous artifact branch), PR base is base_ref_name."""
    stage, state = _make_state_with_gr(
        tmp_path,
        base_ref_name="main",
        impl_materialized_branch="gremlin/child-1",
    )
    prompts_seen: list[str] = []
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
        patch.object(
            stage,
            "run_claude",
            side_effect=lambda prompt, **kw: (
                prompts_seen.append(prompt)
                or type("R", (), {"events": [], "text_result": ""})()
            ),
        ),
    ):
        stage.run(state)
    assert prompts_seen, "run_claude should have been called"
    assert "main" in prompts_seen[0]


def test_run_raises_if_impl_branch_missing(tmp_path: pathlib.Path) -> None:
    stage, state = _make_state_with_gr(tmp_path, base_ref_name="main")
    with (
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        pytest.raises(RuntimeError, match="impl_materialized_branch is empty"),
    ):
        stage.run(state)


def test_explicit_base_ref_used_when_no_prior_pr(tmp_path: pathlib.Path) -> None:
    """Stage-level base_ref is used when there is no prior PR artifact branch."""
    stage = OpenGitHubPR("open-pr", "sonnet", [], {}, base_ref="feature-base")
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id="test-gr",
        base_ref_name="main",
        impl_materialized_branch="gremlin/child-1",
    )
    prompts_seen: list[str] = []
    with (
        patch("gremlins.stages.open_github_pr.last_pr_branch", return_value=None),
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
        patch.object(
            stage,
            "run_claude",
            side_effect=lambda prompt, **kw: (
                prompts_seen.append(prompt)
                or type("R", (), {"events": [], "text_result": ""})()
            ),
        ),
    ):
        stage.run(state)
    assert "feature-base" in prompts_seen[0]


def test_last_pr_branch_takes_priority_over_base_ref(tmp_path: pathlib.Path) -> None:
    """last_pr_branch takes priority over stage-level base_ref when stacking."""
    stage = OpenGitHubPR("open-pr", "sonnet", [], {}, base_ref="feature-base")
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    state = RuntimeState(
        client=client,
        session_dir=tmp_path,
        gr_id="test-gr",
        base_ref_name="main",
        impl_materialized_branch="gremlin/child-2",
    )
    prompts_seen: list[str] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.last_pr_branch",
            return_value="gremlin/child-1",
        ),
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.append_artifact"),
        patch.object(
            stage,
            "run_claude",
            side_effect=lambda prompt, **kw: (
                prompts_seen.append(prompt)
                or type("R", (), {"events": [], "text_result": ""})()
            ),
        ),
    ):
        stage.run(state)
    assert "gremlin/child-1" in prompts_seen[0]
    assert "feature-base" not in prompts_seen[0]


def test_record_child_pr_appends_pr_artifact(tmp_path: pathlib.Path) -> None:
    """After opening a PR, a pr artifact with url and branch is appended."""
    stage, state = _make_state_with_gr(
        tmp_path,
        base_ref_name="main",
        impl_materialized_branch="gremlin/abc-child-1",
    )
    artifact_calls: list[tuple] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.extract_gh_url",
            return_value="https://github.com/owner/repo/pull/314",
        ),
        patch(
            "gremlins.stages.open_github_pr.append_artifact",
            side_effect=lambda gr_id, artifact: artifact_calls.append(
                (gr_id, artifact)
            ),
        ),
    ):
        stage.run(state)
    assert artifact_calls, "append_artifact should be called with PR info"
    gr_id, artifact = artifact_calls[0]
    assert gr_id == "test-gr"
    assert artifact["type"] == "pr"
    assert artifact["url"] == "https://github.com/owner/repo/pull/314"
    assert artifact["branch"] == "gremlin/abc-child-1"
