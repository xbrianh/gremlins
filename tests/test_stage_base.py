"""Tests for Stage base class defaults."""

from __future__ import annotations

from conftest import _TestGremlin
import pathlib
from typing import Any, cast

import pytest

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

    async def run(self, gremlin: State) -> Outcome:
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
        artifact_dir=pathlib.Path("."),
        pipeline_data=_PIPELINE,
    )
    with pytest.raises(NotImplementedError):
        asyncio.run(stage.run(_TestGremlin(state)))


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


def _subs_state() -> State:
    return build_state(
        data=StateData(gremlin_id=None),
        client=FakeClaudeClient(fixtures={}),
        artifact_dir=pathlib.Path("/tmp/sess"),
        pipeline_data=_PIPELINE,
        repo="owner/proj",
        cwd="/work",
        base_ref="trunk",
    )


def test_substitute_vars_renders_shared_framework_keys() -> None:
    stage = _SimpleStage("st", [], {})
    state = _subs_state()
    text = "{name} {model} {artifact_dir} {repo} {cwd} {base_ref}"
    assert stage.substitute_vars(text, _TestGremlin(state)) == (
        "st fake /tmp/sess owner/proj /work trunk"
    )


def test_substitute_vars_framework_wins_over_options_and_extra() -> None:
    stage = _SimpleStage("st", [], {"repo": "from-opt", "x": "opt-x"})
    state = _subs_state()
    out = stage.substitute_vars(
        "{repo} {x} {y}", _TestGremlin(state), extra={"repo": "from-extra", "y": "extra-y"}
    )
    # framework {repo} wins over both option and extra; extra wins over option for {y}.
    assert out == "owner/proj opt-x extra-y"


def test_substitute_vars_extra_wins_over_options() -> None:
    stage = _SimpleStage("st", [], {"k": "opt"})
    state = _subs_state()
    assert stage.substitute_vars("{k}", _TestGremlin(state), extra={"k": "resolved"}) == "resolved"


def test_substitute_vars_unknown_and_nonword_braces_pass_through() -> None:
    stage = _SimpleStage("st", [], {})
    state = _subs_state()
    text = "{unknown} ${shell} {read:k} {{name}}"
    # unknown tokens, shell ${x}, {read:k}, and doubled braces are left verbatim;
    # the inner {name} of {{name}} is substituted (regex, not format_map semantics).
    assert stage.substitute_vars(text, _TestGremlin(state)) == "{unknown} ${shell} {read:k} {st}"
