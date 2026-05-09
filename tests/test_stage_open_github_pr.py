"""Tests for gremlins.stages.open_github_pr."""

from __future__ import annotations

import json
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import StageContext
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


def test_run_patches_pr_url_to_state(tmp_path: pathlib.Path) -> None:
    from unittest.mock import patch as mock_patch

    stage, ctx = _make_stage(tmp_path)
    patched: list[dict] = []
    with (
        mock_patch(
            "gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL
        ),
        mock_patch(
            "gremlins.stages.open_github_pr.patch_state",
            side_effect=lambda gr_id, **kw: patched.append(kw),
        ),
    ):
        stage.run(None)
    assert patched == [{"pr_url": PR_URL}]


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


def test_stacked_pr_uses_prev_child_branch(tmp_path: pathlib.Path) -> None:
    """For child N>1, PR base is the previous child's materialized branch."""
    _write_state(
        tmp_path,
        {
            "base_ref_name": "main",
            "chain_state": {
                "handoff_count": 2,
                "child_records": [
                    {"n": 1, "branch": "gremlin/abc-child-1"},
                ],
            },
        },
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    prompts_seen: list[str] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.resolve_state_file",
            return_value=tmp_path / "state.json",
        ),
        patch(
            "gremlins.stages.open_github_pr.get_prev_child_branch",
            return_value="gremlin/abc-child-1",
        ),
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.upsert_child_record"),
        patch(
            "gremlins.stages.open_github_pr.patch_state",
        ),
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


def test_first_child_uses_base_ref_name(tmp_path: pathlib.Path) -> None:
    """For child 1 (no previous child record), PR base is base_ref_name."""
    _write_state(
        tmp_path,
        {
            "base_ref_name": "main",
            "chain_state": {
                "handoff_count": 1,
                "child_records": [],
            },
        },
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    prompts_seen: list[str] = []
    with (
        patch(
            "gremlins.stages.open_github_pr.resolve_state_file",
            return_value=tmp_path / "state.json",
        ),
        patch("gremlins.stages.open_github_pr.extract_gh_url", return_value=PR_URL),
        patch("gremlins.stages.open_github_pr.patch_state"),
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


def test_record_child_pr_writes_pr_info_to_chain_state(tmp_path: pathlib.Path) -> None:
    """After opening a PR, pr_url and pr_number are stored in child_records."""
    sf = _write_state(
        tmp_path,
        {
            "base_ref_name": "main",
            "chain_state": {
                "handoff_count": 1,
                "child_records": [{"n": 1, "branch": "gremlin/abc-child-1"}],
            },
        },
    )
    stage, ctx = _make_stage_with_gr(tmp_path)
    upsert_calls: list[dict] = []
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
            "gremlins.stages.open_github_pr.upsert_child_record",
            side_effect=lambda gr_id, **kw: upsert_calls.append({"gr_id": gr_id, **kw}),
        ),
        patch("gremlins.stages.open_github_pr.patch_state"),
    ):
        stage.run(None)
    assert upsert_calls, "upsert_child_record should be called with PR info"
    call = upsert_calls[0]
    assert call["gr_id"] == "test-gr"
    assert call["pr_url"] == "https://github.com/owner/repo/pull/314"
    assert call["pr_number"] == 314
