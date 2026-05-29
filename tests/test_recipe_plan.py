"""Tests for plan_arg launcher binding (prereq for plan recipe)."""

from __future__ import annotations

import pathlib

import gremlins.launcher as _launcher


def _make_inputs(*, plan: str | None = None) -> _launcher._Inputs:  # type: ignore[reportPrivateUsage]
    return _launcher._Inputs(  # type: ignore[reportPrivateUsage]
        gremlin_id="test-abc123",
        kind="local",
        plan=plan,
        instructions="",
        description="",
        description_explicit=False,
        parent_id="",
        project_root="/tmp",
        pipeline_path="local",
        pipeline_args=[],
        client_label="claude:sonnet",
        setup_kind="worktree-branch",
        base_ref_name="main",
        base_ref_sha="",
        stage_inputs={},
        pr_num="",
    )


def test_prepare_state_dir_writes_plan_arg_file(tmp_path: pathlib.Path) -> None:
    inputs = _make_inputs(plan="#42")
    _launcher._prepare_state_dir(tmp_path, inputs)  # type: ignore[attr-defined]
    plan_arg_file = tmp_path / "artifacts" / "plan-arg.txt"
    assert plan_arg_file.exists()
    assert plan_arg_file.read_text() == "#42"


def test_prepare_state_dir_writes_empty_plan_arg_when_no_plan(
    tmp_path: pathlib.Path,
) -> None:
    inputs = _make_inputs(plan=None)
    _launcher._prepare_state_dir(tmp_path, inputs)  # type: ignore[attr-defined]
    plan_arg_file = tmp_path / "artifacts" / "plan-arg.txt"
    assert plan_arg_file.exists()
    assert plan_arg_file.read_text() == ""
