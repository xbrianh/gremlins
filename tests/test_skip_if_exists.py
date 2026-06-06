"""Behavior tests for Stage.skip_if_exists dispatch check."""

from __future__ import annotations

import asyncio
import pathlib
from typing import Any

from conftest import MockGremlin

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.stages.agent import Agent
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome

_PIPELINE = Pipeline(
    name="test",
    path=pathlib.Path("."),
    stages=[Agent("stub", [], {})],
)


class _CountingStage(Stage):
    type = "counting"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.run_count = 0

    async def run(self, gremlin) -> Outcome:
        self.run_count += 1
        return Done()


def _make_state(tmp_path: pathlib.Path) -> tuple[State, ArtifactRegistry]:
    reg = ArtifactRegistry(artifact_dir=tmp_path)
    client = FakeClaudeClient(fixtures={})
    state = build_state(
        data=StateData(gremlin_id=None),
        client=client,
        artifact_dir=tmp_path,
        pipeline_data=_PIPELINE,
        artifacts=reg,
    )
    return state, reg


def test_skip_if_exists_skips_when_key_produced(tmp_path: pathlib.Path) -> None:
    state, reg = _make_state(tmp_path)
    (tmp_path / "out.txt").write_text("content", encoding="utf-8")
    reg.bind("my-artifact", Uri.parse("file://session/out.txt"))

    stage = _CountingStage("s", [], {})
    stage.skip_if_exists = "my-artifact"

    gremlin = MockGremlin(state=state)
    runner = state.make_runner(stage, gremlin, record_stage=False)
    asyncio.run(runner())

    assert stage.run_count == 0


def test_skip_if_exists_runs_when_key_absent(tmp_path: pathlib.Path) -> None:
    state, _ = _make_state(tmp_path)

    stage = _CountingStage("s", [], {})
    stage.skip_if_exists = "my-artifact"

    gremlin = MockGremlin(state=state)
    runner = state.make_runner(stage, gremlin, record_stage=False)
    asyncio.run(runner())

    assert stage.run_count == 1


def test_no_skip_if_exists_always_runs(tmp_path: pathlib.Path) -> None:
    state, reg = _make_state(tmp_path)
    reg.mount("my-artifact", Uri.parse("file://session/out.txt"))

    stage = _CountingStage("s", [], {})
    # skip_if_exists is "" by default — should not skip even when key is produced

    gremlin = MockGremlin(state=state)
    runner = state.make_runner(stage, gremlin, record_stage=False)
    asyncio.run(runner())

    assert stage.run_count == 1
