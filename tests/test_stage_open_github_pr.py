"""Tests for gremlins.stages.open_github_pr."""

from __future__ import annotations

import json
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages import StageContext
from gremlins.stages.open_github_pr import OpenGitHubPR

ISSUE_URL = "https://github.com/owner/repo/issues/42"
PR_URL = "https://github.com/owner/repo/pull/42"


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    issue_url: str = ISSUE_URL,
) -> tuple[OpenGitHubPR, StageContext]:
    stage = OpenGitHubPR("open-pr", "sonnet", [], {}, issue_url=issue_url)
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=None)
    stage.bind(ctx)
    return stage, ctx


def test_run_calls_claude_with_push_prompt(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert len(ctx.client.calls) == 1
    call = ctx.client.calls[0]
    assert call.label == "open-github-pr"
    assert "push" in call.prompt.lower()


def test_issue_num_adds_closes_clause(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, issue_url="https://github.com/o/r/issues/42")
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert "Closes #42" in ctx.client.calls[0].prompt


def test_no_issue_url_skips_closes(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, issue_url="")
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert "Include 'Closes" not in ctx.client.calls[0].prompt


def test_run_returns_pr_url(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        result = stage.run(None)
    assert result == PR_URL


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL):
        stage.run(None)
    assert ctx.client.calls[0].raw_path == tmp_path / "stream-open-github-pr.jsonl"


def test_run_raises_if_unbound() -> None:
    stage = OpenGitHubPR("open-pr", None, [], {}, issue_url=ISSUE_URL)
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(None)


def test_run_records_pr_artifact(tmp_path: pathlib.Path) -> None:
    from unittest.mock import patch as mock_patch

    stage, ctx = _make_stage(tmp_path)
    artifact_calls: list[tuple] = []
    with (
        mock_patch(
            "gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL
        ),
        mock_patch(
            "gremlins.stages.open_github_pr.append_artifact",
            side_effect=lambda gr_id, artifact: artifact_calls.append(
                (gr_id, artifact)
            ),
        ),
    ):
        stage.run(None)
    assert len(artifact_calls) == 1
    assert artifact_calls[0][1]["type"] == "pr"
    assert artifact_calls[0][1]["url"] == PR_URL


# ---------------------------------------------------------------------------
# Stacked-PR: chain_state base-ref resolution
# ---------------------------------------------------------------------------


def _make_stage_with_gr(
    tmp_path: pathlib.Path, gr_id: str = "test-gr"
) -> tuple[OpenGitHubPR, StageContext]:
    from gremlins.clients.fake import FakeClaudeClient

    stage = OpenGitHubPR("open-pr", "sonnet", [], {}, issue_url="")
    client = FakeClaudeClient(fixtures={"open-github-pr": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=gr_id)
    stage.bind(ctx)
    return stage, ctx


def _write_state(tmp_path: pathlib.Path, data: dict) -> pathlib.Path:
    sf = tmp_path / "state.json"
    sf.write_text(json.dumps(data), encoding="utf-8")
    return sf


def test_stacked_pr_uses_prior_pr_branch(tmp_path: pathlib.Path) -> None:
    """For child N>1, PR base is the previous child's PR branch."""
    _write_state(
        tmp_path,
        {"base_ref_name": "main", "impl_materialized_branch": "gremlin/child-2"},
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    prompts_seen: list[str] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.resolve_state_file",
            return_value=tmp_path / "state.json",
        ),
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
        stage.run(None)
    assert prompts_seen, "run_claude should have been called"
    assert "gremlin/abc-child-1" in prompts_seen[0], (
        "PR prompt should target previous child branch, not main"
    )


def test_single_pr_with_branch_artifact_uses_base_ref_name(
    tmp_path: pathlib.Path,
) -> None:
    """Regression: a branch artifact (just materialized) must NOT be used as PR base.

    Before the fix, `last_artifact_branch` returned the just-materialized head
    branch, so `gh pr create` was invoked with --base == --head and failed.
    The base for a non-stacked PR must come from base_ref_name.
    """
    _write_state(
        tmp_path,
        {
            "base_ref_name": "main",
            "impl_materialized_branch": "ghgremlin-impl-foo-abc123",
            "artifacts": [{"type": "branch", "name": "ghgremlin-impl-foo-abc123"}],
        },
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    prompts_seen: list[str] = []
    sf = tmp_path / "state.json"
    with (
        patch(
            "gremlins.stages.open_github_pr.resolve_state_file",
            return_value=sf,
        ),
        patch("gremlins.state.resolve_state_file", return_value=sf),
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
        stage.run(None)
    assert prompts_seen, "run_claude should have been called"
    assert "main" in prompts_seen[0]
    assert "ghgremlin-impl-foo-abc123" not in prompts_seen[0], (
        "the just-materialized head branch must not appear as the PR base"
    )


def test_first_child_uses_base_ref_name(tmp_path: pathlib.Path) -> None:
    """For child 1 (no previous artifact branch), PR base is base_ref_name."""
    _write_state(
        tmp_path,
        {"base_ref_name": "main", "impl_materialized_branch": "gremlin/child-1"},
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    prompts_seen: list[str] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.resolve_state_file",
            return_value=tmp_path / "state.json",
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
        stage.run(None)
    assert prompts_seen, "run_claude should have been called"
    assert "main" in prompts_seen[0]


def test_record_child_pr_appends_pr_artifact(tmp_path: pathlib.Path) -> None:
    """After opening a PR, a pr artifact with url and branch is appended."""
    sf = _write_state(
        tmp_path,
        {"base_ref_name": "main", "impl_materialized_branch": "gremlin/abc-child-1"},
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    artifact_calls: list[tuple] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.resolve_state_file",
            return_value=sf,
        ),
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
        stage.run(None)
    assert artifact_calls, "append_artifact should be called with PR info"
    gr_id, artifact = artifact_calls[0]
    assert gr_id == "test-gr"
    assert artifact["type"] == "pr"
    assert artifact["url"] == "https://github.com/owner/repo/pull/314"
    assert artifact["branch"] == "gremlin/abc-child-1"
