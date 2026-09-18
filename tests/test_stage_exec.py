"""Tests for gremlins.stages.exec.Exec interpolation resolution."""

from __future__ import annotations

import asyncio
import pathlib

import pytest
from _gremlins_core.artifacts import MissingArtifact
from _gremlins_core.executor import StateData, build_state
from _gremlins_core.stages import Exec
from conftest import MockGremlin

from tests.fake_client import FakeClient


def _make_state(tmp_path: pathlib.Path):
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=FakeClient(),
        artifact_dir=artifact_dir,
        worktree=tmp_path,
    )


def test_interpolation_map_missing_artifact_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = Exec("test", {"cmds": ["true"]}, interpolation_map={"X": "not-bound"})
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(MockGremlin(state=state)))
