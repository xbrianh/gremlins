"""Tests for gremlins.stages.handoff_branch."""

from __future__ import annotations

import json
import pathlib
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from gremlins.git import DirtyOnly, DivergentHead, EmptyImpl, HeadAdvanced, PreImplState
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.handoff_branch import HandoffBranch, HandoffBranchResult

PRE_STATE = PreImplState(head="abc123", branch="main")


def _make_entry() -> StageEntry:
    return StageEntry(
        name="handoff-branch",
        type="handoff-branch",
        client=None,
        prompt_paths=[],
        options={},
    )


def _make_stage(
    tmp_path: pathlib.Path, gr_id: str | None = None
) -> tuple[HandoffBranch, StageContext]:
    entry = _make_entry()
    stage = HandoffBranch(entry, None)
    from gremlins.clients.fake import FakeClaudeClient

    ctx = StageContext(client=FakeClaudeClient(), session_dir=tmp_path, gr_id=gr_id)
    stage.bind(ctx)
    return stage, ctx


def _pipe(pre_state: PreImplState | None = PRE_STATE) -> SimpleNamespace:
    return SimpleNamespace(impl_pre_state=pre_state)


def test_head_advanced_creates_handoff_branch(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    outcome = HeadAdvanced(commit_count=2)
    with (
        patch(
            "gremlins.stages.handoff_branch.classify_impl_outcome", return_value=outcome
        ),
        patch(
            "gremlins.stages.handoff_branch.create_handoff_branch",
            return_value="ghgremlin-impl-handoff-1234",
        ) as mock_create,
        patch("gremlins.stages.handoff_branch.reset_pre_branch") as mock_reset,
        patch(
            "gremlins.stages.handoff_branch.sweep_stale_handoff_branches"
        ) as mock_sweep,
        patch("gremlins.stages.handoff_branch.patch_state"),
    ):
        result = stage.run(_pipe())
    assert isinstance(result, HandoffBranchResult)
    assert result.handoff_branch == "ghgremlin-impl-handoff-1234"
    assert result.base_ref == "abc123"
    assert isinstance(result.outcome, HeadAdvanced)
    mock_create.assert_called_once()
    mock_reset.assert_called_once()
    mock_sweep.assert_called_once()


def test_dirty_only_no_branch_created(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    with (
        patch(
            "gremlins.stages.handoff_branch.classify_impl_outcome",
            return_value=DirtyOnly(),
        ),
        patch("gremlins.stages.handoff_branch.create_handoff_branch") as mock_create,
        patch("gremlins.stages.handoff_branch.patch_state"),
    ):
        result = stage.run(_pipe())
    assert result.handoff_branch == ""
    assert result.base_ref == "abc123"
    mock_create.assert_not_called()


def test_empty_impl_raises(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    with patch(
        "gremlins.stages.handoff_branch.classify_impl_outcome", return_value=EmptyImpl()
    ):
        with pytest.raises(RuntimeError, match="no changes"):
            stage.run(_pipe())


def test_divergent_head_raises(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    outcome = DivergentHead(pre_head="abc123", post_head="def456")
    with patch(
        "gremlins.stages.handoff_branch.classify_impl_outcome", return_value=outcome
    ):
        with pytest.raises(RuntimeError, match="without advancing"):
            stage.run(_pipe())


def test_none_pre_state_no_state_file_raises(tmp_path: pathlib.Path) -> None:
    # gr_id=None → resolve_state_file returns None → RuntimeError
    stage, _ = _make_stage(tmp_path, gr_id=None)
    with pytest.raises(RuntimeError, match="rewind to implement"):
        stage.run(_pipe(None))


def test_none_pre_state_reads_from_state_json(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(
        json.dumps({"impl_pre_head": "feed1234", "impl_pre_branch": "feat/x"}),
        encoding="utf-8",
    )
    stage, _ = _make_stage(tmp_path, gr_id="test-gr")
    with (
        patch(
            "gremlins.stages.handoff_branch.resolve_state_file",
            return_value=state_file,
        ),
        patch(
            "gremlins.stages.handoff_branch.classify_impl_outcome",
            return_value=DirtyOnly(),
        ),
        patch("gremlins.stages.handoff_branch.patch_state"),
    ):
        result = stage.run(_pipe(None))
    assert result.base_ref == "feed1234"
    assert result.handoff_branch == ""


def test_none_pre_state_missing_head_raises(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(json.dumps({}), encoding="utf-8")
    stage, _ = _make_stage(tmp_path, gr_id="test-gr")
    with (
        patch(
            "gremlins.stages.handoff_branch.resolve_state_file",
            return_value=state_file,
        ),
    ):
        with pytest.raises(RuntimeError, match="impl_pre_head missing"):
            stage.run(_pipe(None))


def test_run_writes_to_state_json(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    outcome = HeadAdvanced(commit_count=1)
    with (
        patch(
            "gremlins.stages.handoff_branch.classify_impl_outcome", return_value=outcome
        ),
        patch(
            "gremlins.stages.handoff_branch.create_handoff_branch",
            return_value="ghgremlin-impl-handoff-42",
        ),
        patch("gremlins.stages.handoff_branch.reset_pre_branch"),
        patch("gremlins.stages.handoff_branch.sweep_stale_handoff_branches"),
        patch("gremlins.stages.handoff_branch.patch_state") as mock_patch,
    ):
        result = stage.run(_pipe())
    mock_patch.assert_called_once_with(
        None,
        impl_handoff_branch="ghgremlin-impl-handoff-42",
        impl_base_ref="abc123",
    )
    assert result.handoff_branch == "ghgremlin-impl-handoff-42"


def test_result_base_ref_from_pre_state(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    pre = PreImplState(head="deadbeef", branch="feature")
    with (
        patch(
            "gremlins.stages.handoff_branch.classify_impl_outcome",
            return_value=DirtyOnly(),
        ),
        patch("gremlins.stages.handoff_branch.patch_state"),
    ):
        result = stage.run(_pipe(pre))
    assert result.base_ref == "deadbeef"
