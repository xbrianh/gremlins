"""Tests for Stage base class defaults."""

from __future__ import annotations

import pathlib
from typing import TYPE_CHECKING, Any, cast

import pytest
from _gremlins_core.schemas import Pipeline
from _gremlins_core.stages import Agent, Done, Outcome
from conftest import MockGremlin

from gremlins.executor.state import StateData, build_state
from gremlins.stages.base import Stage
from tests.fake_client import FakeClient

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin

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

    async def run(self, gremlin: Gremlin) -> Outcome:  # type: ignore[override]
        return Done()


def test_stage_init_takes_only_name() -> None:
    stage = Stage("my-stage")
    assert stage.name == "my-stage"
    assert stage.client is None
    assert stage.path == ""


def test_stage_run_raises_not_implemented() -> None:
    import asyncio

    stage = Stage("my-stage")
    client = FakeClient(fixtures={})
    state = build_state(
        data=StateData(gremlin_id=None),
        client=client,
        artifact_dir=pathlib.Path("."),
        pipeline_data=_PIPELINE,
    )
    with pytest.raises(NotImplementedError):
        gremlin = cast("Gremlin", MockGremlin(state))
        asyncio.run(stage.run(gremlin))


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
