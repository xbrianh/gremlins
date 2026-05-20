"""Tests for gremlins.stages.record_pr_artifact."""

from __future__ import annotations

import asyncio
import json
import pathlib
import subprocess
from unittest.mock import AsyncMock, patch

from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.outcome import Done
from gremlins.stages.record_pr_artifact import RecordPrArtifact, pr_num_from_ref

PR_URL = "https://github.com/owner/repo/pull/42"
PR_BRANCH = "issue-42-some-work"
_GH_RESPONSE = json.dumps({"url": PR_URL, "headRefName": PR_BRANCH})


def _make(
    tmp_path: pathlib.Path,
    *,
    worktree_base: str = "",
    base_ref_sha: str = "",
    gremlin_id: str | None = "test-gr",
) -> tuple[RecordPrArtifact, RuntimeState]:
    stage = RecordPrArtifact("record-pr-artifact")
    state = RuntimeState(
        data=StateData(
            gremlin_id=gremlin_id,
            worktree_base=worktree_base,
            base_ref_sha=base_ref_sha,
        ),
        client=None,  # type: ignore[arg-type]
        session_dir=tmp_path,
    )
    return stage, state


def _ok(stdout: str = _GH_RESPONSE) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess([], 0, stdout, "")


def _fail() -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess([], 1, "", "not found")


# --- _pr_num_from_ref unit tests ---


def test_pr_num_from_ref_standard() -> None:
    assert pr_num_from_ref("pull/42/head") == "42"


def test_pr_num_from_ref_no_match() -> None:
    assert pr_num_from_ref("main") == ""


def test_pr_num_from_ref_empty() -> None:
    assert pr_num_from_ref("") == ""


# --- RecordPrArtifact.run tests ---


def test_records_artifact_from_worktree_base(tmp_path: pathlib.Path) -> None:
    stage, state = _make(tmp_path, worktree_base="pull/42/head")
    with (
        patch(
            "gremlins.stages.record_pr_artifact.proc.run_async",
            new=AsyncMock(return_value=_ok()),
        ),
        patch("gremlins.executor.state.StateData.append_artifact") as mock_append,
    ):
        result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    mock_append.assert_called_once_with(
        {"type": "pr", "url": PR_URL, "branch": PR_BRANCH}
    )


def test_falls_back_to_base_ref_sha(tmp_path: pathlib.Path) -> None:
    stage, state = _make(tmp_path, worktree_base="", base_ref_sha="pull/99/head")
    with (
        patch(
            "gremlins.stages.record_pr_artifact.proc.run_async",
            new=AsyncMock(return_value=_ok()),
        ) as mock_run,
        patch("gremlins.executor.state.StateData.append_artifact"),
    ):
        asyncio.run(stage.run(state))
    cmd = mock_run.call_args[0][0]
    assert cmd[3] == "99"


def test_no_pr_ref_returns_done_without_artifact(tmp_path: pathlib.Path) -> None:
    stage, state = _make(tmp_path, worktree_base="main", base_ref_sha="")
    with (
        patch(
            "gremlins.stages.record_pr_artifact.proc.run_async",
            new=AsyncMock(return_value=_ok()),
        ) as mock_run,
        patch("gremlins.executor.state.StateData.append_artifact") as mock_append,
    ):
        result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    mock_run.assert_not_called()
    mock_append.assert_not_called()


def test_gh_failure_returns_done_without_artifact(tmp_path: pathlib.Path) -> None:
    stage, state = _make(tmp_path, worktree_base="pull/42/head")
    with (
        patch(
            "gremlins.stages.record_pr_artifact.proc.run_async",
            new=AsyncMock(return_value=_fail()),
        ),
        patch("gremlins.executor.state.StateData.append_artifact") as mock_append,
    ):
        result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    mock_append.assert_not_called()
