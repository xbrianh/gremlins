"""Tests for gremlins.stages.commit."""

from __future__ import annotations

import json
import pathlib
from unittest.mock import patch

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.git import DirtyOnly, GitError, HeadAdvanced, ImplOutcome
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.commit import Commit

ISSUE_URL = "https://github.com/owner/repo/issues/42"
HANDOFF_BRANCH = "impl/some-feature"
BASE_REF = "abc123"


def _make_entry() -> StageEntry:
    return StageEntry(
        name="commit",
        type="commit",
        client=None,
        prompt_paths=[],
        options={},
    )


def _make_stage(
    tmp_path: pathlib.Path,
    *,
    impl_outcome: ImplOutcome | None = None,
    impl_handoff_branch: str = HANDOFF_BRANCH,
    base_ref: str = BASE_REF,
    issue_url: str = ISSUE_URL,
) -> tuple[Commit, StageContext]:
    if impl_outcome is None:
        impl_outcome = DirtyOnly()
    entry = _make_entry()
    stage = Commit(
        entry,
        "sonnet",
        impl_outcome=impl_outcome,
        impl_handoff_branch=impl_handoff_branch,
        base_ref=base_ref,
        issue_url=issue_url,
    )
    client = FakeClaudeClient(fixtures={"commit": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=None)
    stage.bind(ctx)
    return stage, ctx


def test_run_calls_claude_with_diff(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.diff_output", return_value="fake diff content"),
        patch("gremlins.stages.commit.log_patch", return_value="fake diff content"),
    ):
        stage.run(None)
    assert len(ctx.client.calls) == 1
    call = ctx.client.calls[0]
    assert call.label == "commit"
    assert "fake diff content" in call.prompt


def test_fresh_case_uses_fresh_prompt_content(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, impl_outcome=DirtyOnly())
    with patch("gremlins.stages.commit.diff_output", return_value="diff"):
        stage.run(None)
    prompt = ctx.client.calls[0].prompt
    assert "Create a new branch" in prompt


def test_handoff_clean_case_includes_branch_name(tmp_path: pathlib.Path) -> None:
    outcome = HeadAdvanced(commit_count=2)
    stage, ctx = _make_stage(tmp_path, impl_outcome=outcome)
    with (
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
        patch("gremlins.stages.commit.has_dirty_worktree", return_value=False),
    ):
        stage.run(None)
    prompt = ctx.client.calls[0].prompt
    assert HANDOFF_BRANCH in prompt
    assert "already committed" in prompt


def test_handoff_dirty_case_includes_branch_name(tmp_path: pathlib.Path) -> None:
    outcome = HeadAdvanced(commit_count=1)
    stage, ctx = _make_stage(tmp_path, impl_outcome=outcome)
    with (
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
        patch("gremlins.stages.commit.has_dirty_worktree", return_value=True),
    ):
        stage.run(None)
    prompt = ctx.client.calls[0].prompt
    assert HANDOFF_BRANCH in prompt
    assert "partially committed" in prompt


def test_issue_num_adds_closes_clause(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, issue_url="https://github.com/o/r/issues/42")
    with patch("gremlins.stages.commit.diff_output", return_value="diff"):
        stage.run(None)
    assert "Closes #42" in ctx.client.calls[0].prompt


def test_no_issue_url_skips_closes(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path, issue_url="")
    with patch("gremlins.stages.commit.diff_output", return_value="diff"):
        stage.run(None)
    assert "End the commit message with 'Closes" not in ctx.client.calls[0].prompt


def test_run_writes_raw_path(tmp_path: pathlib.Path) -> None:
    stage, ctx = _make_stage(tmp_path)
    with patch("gremlins.stages.commit.diff_output", return_value="diff"):
        stage.run(None)
    assert ctx.client.calls[0].raw_path == tmp_path / "stream-commit.jsonl"


def test_run_raises_if_unbound() -> None:
    entry = _make_entry()
    stage = Commit(
        entry,
        None,
        impl_outcome=DirtyOnly(),
        impl_handoff_branch=HANDOFF_BRANCH,
        base_ref=BASE_REF,
        issue_url=ISSUE_URL,
    )
    with pytest.raises(RuntimeError, match="not bound"):
        stage.run(None)


# ---------------------------------------------------------------------------
# Self-sourcing tests (impl_outcome=None, reads from state.json)
# ---------------------------------------------------------------------------


def _make_self_sourcing_stage(
    tmp_path: pathlib.Path,
) -> tuple[Commit, StageContext]:
    entry = _make_entry()
    stage = Commit(entry, "sonnet")
    client = FakeClaudeClient(fixtures={"commit": MINIMAL_EVENTS})
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id="test-gr")
    stage.bind(ctx)
    return stage, ctx


def test_self_source_head_advanced(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_handoff_branch": HANDOFF_BRANCH,
                "impl_base_ref": BASE_REF,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, ctx = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        patch("gremlins.stages.commit.rev_list_count", return_value=3),
        patch("gremlins.stages.commit.log_patch", return_value="diff"),
        patch("gremlins.stages.commit.has_dirty_worktree", return_value=False),
    ):
        stage.run(None)
    prompt = ctx.client.calls[0].prompt
    assert HANDOFF_BRANCH in prompt
    assert "Closes #42" in prompt


def test_self_source_dirty_only(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_handoff_branch": "",
                "impl_base_ref": BASE_REF,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, ctx = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        patch("gremlins.stages.commit.diff_output", return_value="diff"),
    ):
        stage.run(None)
    prompt = ctx.client.calls[0].prompt
    assert HANDOFF_BRANCH not in prompt


def test_self_source_raises_without_base_ref(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_handoff_branch": HANDOFF_BRANCH,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, ctx = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        pytest.raises(RuntimeError, match="no impl_base_ref"),
    ):
        stage.run(None)


def test_self_source_rev_list_error(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps(
            {
                "impl_handoff_branch": HANDOFF_BRANCH,
                "impl_base_ref": BASE_REF,
                "issue_url": ISSUE_URL,
            }
        )
    )
    stage, ctx = _make_self_sourcing_stage(tmp_path)
    with (
        patch("gremlins.stages.commit.resolve_state_file", return_value=state_file),
        patch(
            "gremlins.stages.commit.rev_list_count",
            side_effect=GitError(128, "bad revision"),
        ),
        pytest.raises(RuntimeError),
    ):
        stage.run(None)
