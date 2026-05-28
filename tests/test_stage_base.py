"""Tests for Stage base class defaults."""

from __future__ import annotations

import asyncio
import pathlib
from typing import Any

import pytest

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


class _SimpleStage(Stage):
    type = "simple"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options

    async def run(self, state: State) -> Outcome:
        return Done()


def test_stage_init_takes_only_name() -> None:
    stage = Stage("my-stage")
    assert stage.name == "my-stage"
    assert stage.client is None
    assert stage.path == ""


def test_stage_run_raises_not_implemented() -> None:
    import asyncio

    stage = Stage("my-stage")
    client = FakeClaudeClient(fixtures={})
    state = build_state(
        data=StateData(gremlin_id=None),
        client=client,
        session_dir=pathlib.Path("."),
        pipeline_data=_PIPELINE,
    )
    with pytest.raises(NotImplementedError):
        asyncio.run(stage.run(state))


def test_default_with_dict_constructs_subclass() -> None:
    d = {"name": "my-simple", "prompt": ["do stuff"], "options": {"foo": "bar"}}
    stage = _SimpleStage.with_dict(d)
    assert isinstance(stage, _SimpleStage)
    assert stage.name == "my-simple"
    assert stage.prompts == ["do stuff"]
    assert stage.options == {"foo": "bar"}


def test_default_with_dict_sets_client() -> None:
    d = {"name": "my-simple", "prompt": [], "options": {}}
    stage = _SimpleStage.with_dict(d)
    # No client key in dict — client is set to None or the default.
    # get_client_from_dict returns None when no client key present.
    assert stage.client is None


def test_path_setter_propagates_to_body() -> None:
    """path setter does not error on leaf stages (no body attribute)."""
    stage = _SimpleStage("leaf", [], {})
    stage.path = "parent/leaf"
    assert stage.path == "parent/leaf"


def test_deleted_helpers_not_on_stage() -> None:
    stage = Stage("s")
    assert not hasattr(stage, "run_claude")
    assert not hasattr(stage, "bail_command")
    assert not hasattr(stage, "run_subprocess")


def test_skip_if_exists_skips_when_key_produced(tmp_path: pathlib.Path) -> None:
    ran: list[bool] = []

    class _TrackingStage(_SimpleStage):
        async def run(self, state: State) -> Outcome:
            ran.append(True)
            return Done()

    stage = _TrackingStage("s", [], {})
    stage.skip_if_exists = "my-key"

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    state = build_state(
        data=StateData(gremlin_id=None),
        client=FakeClaudeClient(fixtures={}),
        session_dir=session_dir,
        pipeline_data=_PIPELINE,
    )
    state.artifacts.bind("my-key", Uri.parse("file://session/plan.md"))

    runner = state.make_runner(stage, record_stage=False)
    asyncio.run(runner())

    assert ran == []


def test_skip_if_exists_runs_when_key_absent(tmp_path: pathlib.Path) -> None:
    ran: list[bool] = []

    class _TrackingStage(_SimpleStage):
        async def run(self, state: State) -> Outcome:
            ran.append(True)
            return Done()

    stage = _TrackingStage("s", [], {})
    stage.skip_if_exists = "my-key"

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    state = build_state(
        data=StateData(gremlin_id=None),
        client=FakeClaudeClient(fixtures={}),
        session_dir=session_dir,
        pipeline_data=_PIPELINE,
    )

    runner = state.make_runner(stage, record_stage=False)
    asyncio.run(runner())

    assert ran == [True]
