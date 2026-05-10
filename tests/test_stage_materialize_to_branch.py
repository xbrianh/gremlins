"""Tests for gremlins.stages.materialize_to_branch."""

from __future__ import annotations

import json
import pathlib
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from gremlins.git import DirtyOnly, DivergentHead, EmptyImpl, HeadAdvanced, PreImplState
from gremlins.stages import StageContext
from gremlins.stages.materialize_to_branch import (
    MaterializeToBranch,
    MaterializeToBranchResult,
)

PRE_STATE = PreImplState(head="abc123", branch="main")


def _make_stage(
    tmp_path: pathlib.Path, gr_id: str | None = None
) -> tuple[MaterializeToBranch, StageContext]:
    stage = MaterializeToBranch("materialize-to-branch", None, [], {})
    from gremlins.clients.fake import FakeClaudeClient

    ctx = StageContext(client=FakeClaudeClient(), session_dir=tmp_path, gr_id=gr_id)
    stage.bind(ctx)
    return stage, ctx


def _pipe(pre_state: PreImplState | None = PRE_STATE) -> SimpleNamespace:
    return SimpleNamespace(impl_pre_state=pre_state)


def test_head_advanced_creates_materialized_branch(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    outcome = HeadAdvanced(commit_count=2)
    with (
        patch(
            "gremlins.stages.materialize_to_branch.classify_impl_outcome",
            return_value=outcome,
        ),
        patch(
            "gremlins.stages.materialize_to_branch.create_handoff_branch",
            return_value="ghgremlin-impl-handoff-1234",
        ) as mock_create,
        patch("gremlins.stages.materialize_to_branch.reset_pre_branch") as mock_reset,
        patch(
            "gremlins.stages.materialize_to_branch.sweep_stale_handoff_branches"
        ) as mock_sweep,
        patch("gremlins.stages.materialize_to_branch.patch_state"),
    ):
        result = stage.run(_pipe())
    assert isinstance(result, MaterializeToBranchResult)
    assert result.materialized_branch == "ghgremlin-impl-handoff-1234"
    assert result.base_ref == "abc123"
    assert isinstance(result.outcome, HeadAdvanced)
    mock_create.assert_called_once()
    mock_reset.assert_called_once()
    mock_sweep.assert_called_once()


def test_dirty_only_raises(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    with (
        patch(
            "gremlins.stages.materialize_to_branch.classify_impl_outcome",
            return_value=DirtyOnly(),
        ),
        patch("gremlins.stages.materialize_to_branch.create_handoff_branch") as mock_create,
    ):
        with pytest.raises(RuntimeError, match="unexpected impl outcome"):
            stage.run(_pipe())
    mock_create.assert_not_called()


def test_empty_impl_raises(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    with patch(
        "gremlins.stages.materialize_to_branch.classify_impl_outcome",
        return_value=EmptyImpl(),
    ):
        with pytest.raises(RuntimeError, match="no changes"):
            stage.run(_pipe())


def test_divergent_head_raises(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    outcome = DivergentHead(pre_head="abc123", post_head="def456")
    with patch(
        "gremlins.stages.materialize_to_branch.classify_impl_outcome",
        return_value=outcome,
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
            "gremlins.stages.materialize_to_branch.resolve_state_file",
            return_value=state_file,
        ),
        patch(
            "gremlins.stages.materialize_to_branch.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
        patch(
            "gremlins.stages.materialize_to_branch.create_handoff_branch",
            return_value="ghgremlin-impl-handoff-99",
        ),
        patch("gremlins.stages.materialize_to_branch.reset_pre_branch"),
        patch("gremlins.stages.materialize_to_branch.sweep_stale_handoff_branches"),
        patch("gremlins.stages.materialize_to_branch.patch_state"),
        patch("gremlins.stages.materialize_to_branch.append_artifact"),
    ):
        result = stage.run(_pipe(None))
    assert result.base_ref == "feed1234"
    assert result.materialized_branch == "ghgremlin-impl-handoff-99"


def test_none_pre_state_missing_head_raises(tmp_path: pathlib.Path) -> None:
    state_file = tmp_path / "state.json"
    state_file.write_text(json.dumps({}), encoding="utf-8")
    stage, _ = _make_stage(tmp_path, gr_id="test-gr")
    with (
        patch(
            "gremlins.stages.materialize_to_branch.resolve_state_file",
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
            "gremlins.stages.materialize_to_branch.classify_impl_outcome",
            return_value=outcome,
        ),
        patch(
            "gremlins.stages.materialize_to_branch.create_handoff_branch",
            return_value="ghgremlin-impl-handoff-42",
        ),
        patch("gremlins.stages.materialize_to_branch.reset_pre_branch"),
        patch("gremlins.stages.materialize_to_branch.sweep_stale_handoff_branches"),
        patch("gremlins.stages.materialize_to_branch.patch_state") as mock_patch,
        patch("gremlins.stages.materialize_to_branch.append_artifact"),
    ):
        result = stage.run(_pipe())
    mock_patch.assert_called_once_with(
        None,
        impl_materialized_branch="ghgremlin-impl-handoff-42",
        impl_base_ref="abc123",
    )
    assert result.materialized_branch == "ghgremlin-impl-handoff-42"


def test_result_base_ref_from_pre_state(tmp_path: pathlib.Path) -> None:
    stage, _ = _make_stage(tmp_path)
    pre = PreImplState(head="deadbeef", branch="feature")
    with (
        patch(
            "gremlins.stages.materialize_to_branch.classify_impl_outcome",
            return_value=HeadAdvanced(commit_count=1),
        ),
        patch(
            "gremlins.stages.materialize_to_branch.create_handoff_branch",
            return_value="ghgremlin-impl-handoff-77",
        ),
        patch("gremlins.stages.materialize_to_branch.reset_pre_branch"),
        patch("gremlins.stages.materialize_to_branch.sweep_stale_handoff_branches"),
        patch("gremlins.stages.materialize_to_branch.patch_state"),
        patch("gremlins.stages.materialize_to_branch.append_artifact"),
    ):
        result = stage.run(_pipe(pre))
    assert result.base_ref == "deadbeef"


def test_head_advanced_records_branch_artifact(tmp_path: pathlib.Path) -> None:
    """When HeadAdvanced, the materialized branch is recorded as a branch artifact."""
    state_file = tmp_path / "state.json"
    state_file.write_text(json.dumps({"impl_pre_head": "abc123"}), encoding="utf-8")
    stage, _ = _make_stage(tmp_path, gr_id="test-gr")
    artifact_calls: list[tuple[str | None, dict[str, str]]] = []

    def _capture(gr_id: str | None, artifact: dict[str, str]) -> None:
        artifact_calls.append((gr_id, artifact))

    outcome = HeadAdvanced(commit_count=1)
    with (
        patch(
            "gremlins.stages.materialize_to_branch.classify_impl_outcome",
            return_value=outcome,
        ),
        patch(
            "gremlins.stages.materialize_to_branch.create_handoff_branch",
            return_value="gremlin/child-2",
        ),
        patch("gremlins.stages.materialize_to_branch.reset_pre_branch"),
        patch("gremlins.stages.materialize_to_branch.sweep_stale_handoff_branches"),
        patch(
            "gremlins.stages.materialize_to_branch.resolve_state_file",
            return_value=state_file,
        ),
        patch("gremlins.stages.materialize_to_branch.patch_state"),
        patch(
            "gremlins.stages.materialize_to_branch.append_artifact",
            side_effect=_capture,
        ),
    ):
        result = stage.run(_pipe())
    assert result.materialized_branch == "gremlin/child-2"
    assert artifact_calls, "append_artifact should be called"
    assert artifact_calls[0] == (
        "test-gr",
        {"type": "branch", "name": "gremlin/child-2"},
    )
