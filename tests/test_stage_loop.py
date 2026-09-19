"""Tests for LoopStage framework substitution and validation."""

from __future__ import annotations

from typing import Any

import pytest
from _gremlins_core.executor import State as RuntimeState
from _gremlins_core.executor import StateData, build_state
from _gremlins_core.stages import Loop as LoopStage


def _fake_client() -> Any:
    from tests.fake_client import FakeClient

    return FakeClient(fixtures={})


def _loop_state(tmp_path: Any) -> RuntimeState:
    (tmp_path / "artifacts").mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=_fake_client(),
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )


def test_loop_iter_not_in_framework_subs(tmp_path):
    """loop_iter and loop_iteration are absent from framework_subs."""
    loop_state = _loop_state(tmp_path)
    stage = LoopStage("test", body_runners=[], max_iterations=1)
    subs = loop_state.framework_subs(stage)
    assert "loop_iter" not in subs
    assert "loop_iteration" not in subs


def test_max_iterations_setter_validates() -> None:
    loop = LoopStage("test", body_runners=[])
    loop.max_iterations = 5
    assert loop.max_iterations == 5
    with pytest.raises(ValueError):
        loop.max_iterations = 0
    assert loop.max_iterations == 5
