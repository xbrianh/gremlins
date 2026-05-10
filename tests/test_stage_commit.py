"""Tests for gremlins.stages.commit."""

from __future__ import annotations

import json
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.git import GitError
from gremlins.stages.base import RuntimeState
from gremlins.stages.commit import Commit

ISSUE_URL = "https://github.com/owner/repo/issues/42"
MATERIALIZED_BRANCH = "impl/some-feature"
BASE_REF = "abc123"


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    impl_materialized_branch: str = MATERIALIZED_BRANCH,
    base_ref: str = BASE_REF,
    issue_url: str = ISSUE_URL,
) -> tuple[Commit, RuntimeState]:
    stage = Commit(
        "commit",
        "sonnet",
        [],
        {},
        impl_materialized_branch=impl_materialized_branch,
        base_ref=base_ref,
        issue_url=issue_url,
    )
    client = FakeClaudeClient(fixtures={"commit": MINIMAL_EVENTS})
    state = RuntimeState(client=client, session_dir=tmp_path, gr_id=None)
    return stage, state


def test_run_calls_claude_with_diff(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.rev_list_count", return_value=1),
        patch("gremlins.stages.commit.log_patch", return_value="fake diff content"),
    ):
        stage.run(state)
    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "commit"
    assert "fake diff content" in call.prompt


def test_handoff_case_includes_branch_name(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.rev_list_count", return_value=2),
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
    ):
        stage.run(state)
    prompt = state.client.calls[0].prompt
    assert MATERIALIZED_BRANCH in prompt
    assert "already committed" in prompt


def test_issue_num_adds_closes_clause(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path, issue_url="https://github.com/o/r/issues/42")
    with (
        patch("gremlins.stages.commit.rev_list_count", return_value=1),
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
    ):
        stage.run(state)
    assert "Closes #42" in state.client.calls[0].prompt


def test_no_issue_url_skips_closes(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path, issue_url="")
    with (
        patch("gremlins.stages.commit.rev_list_count", return_value=1),
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
    ):
        stage.run(state)
    assert "End the commit message with 'Closes" not in state.client.calls[0].prompt


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.rev_list_count", return_value=1),
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
    ):
        stage.run(state)
    assert state.client.calls[0].raw_path == tmp_path / "stream-commit.jsonl"


def test_rev_list_error_raises(tmp_path: pathlib.Path) -> None:
    stage, state = _make_stage(tmp_path)
    with patch(
        "gremlins.stages.commit.rev_list_count",
        side_effect=GitError(128, "bad revision"),
    ):
        with pytest.raises(RuntimeError):
            stage.run(state)


# ---------------------------------------------------------------------------
# Self-sourcing tests (reads from state.json)
# ---------------------------------------------------------------------------


def _make_self_sourcing_stage(
    tmp_path: pathlib.Path,
) -> tuple[Commit, RuntimeState]:
    stage = Commit("commit", "sonnet", [], {})
    client = FakeClaudeClient(fixtures={"commit": MINIMAL_EVENTS})
    state = RuntimeState(client=client, session_dir=tmp_path, gr_id="test-gr")
    return stage, state


def test_self_source_head_advanced(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_materialized_branch": MATERIALIZED_BRANCH,
                "impl_base_ref": BASE_REF,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, state = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        patch("gremlins.stages.commit.rev_list_count", return_value=3),
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
    ):
        stage.run(state)
    prompt = state.client.calls[0].prompt
    assert MATERIALIZED_BRANCH in prompt
    assert "Closes #42" in prompt


def test_self_source_raises_without_materialized_branch(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_materialized_branch": "",
                "impl_base_ref": BASE_REF,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, state = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        pytest.raises(RuntimeError, match="no impl_materialized_branch"),
    ):
        stage.run(state)


def test_self_source_raises_without_base_ref(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_materialized_branch": MATERIALIZED_BRANCH,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, state = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        pytest.raises(RuntimeError, match="no impl_base_ref"),
    ):
        stage.run(state)


def test_self_source_rev_list_error(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_materialized_branch": MATERIALIZED_BRANCH,
                "impl_base_ref": BASE_REF,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, state = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        patch(
            "gremlins.stages.commit.rev_list_count",
            side_effect=GitError(128, "bad revision"),
        ),
        pytest.raises(RuntimeError),
    ):
        stage.run(state)
