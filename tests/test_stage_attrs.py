"""Tests for StageAttrs defaults and get_client_from_dict."""

from __future__ import annotations

import pathlib

import pytest
from _gremlins_core.clients import Client
from _gremlins_core.executor import StateData, build_state
from _gremlins_core.stages import StageAttrs, child_state, get_client_from_dict

from tests.fake_client import FakeClient


def test_stage_attrs_init_takes_only_name() -> None:
    stage = StageAttrs("my-stage")
    assert stage.name == "my-stage"
    assert stage.client is None
    assert stage.path == ""
    assert stage.body == []
    assert stage.options == {}
    assert stage.bind_map == {}


def test_deleted_helpers_not_on_stage() -> None:
    stage = StageAttrs("s")
    assert not hasattr(stage, "run_claude")
    assert not hasattr(stage, "bail_command")
    assert not hasattr(stage, "run_subprocess")


def test_path_setter_propagates_to_body() -> None:
    """path setter does not error on leaf stages (no body attribute)."""
    stage = StageAttrs("leaf")
    stage.path = "parent/leaf"
    assert stage.path == "parent/leaf"


def test_path_setter_rewrites_child_paths() -> None:
    children = [StageAttrs("a"), StageAttrs("b")]
    stage = StageAttrs("parent")
    stage.body = list(children)
    stage.path = "pipeline/parent"
    assert [c.path for c in children] == ["pipeline/parent/a", "pipeline/parent/b"]


def test_options_and_bind_map_persist() -> None:
    stage = StageAttrs("s")
    stage.options = {"max_iterations": 3}
    stage.bind_map = {"out?": "artifact://out.txt"}
    stage.options["interval"] = 1.5
    assert stage.options == {"max_iterations": 3, "interval": 1.5}
    assert stage.bind_map == {"out?": "artifact://out.txt"}


def test_get_client_from_dict_absent() -> None:
    assert get_client_from_dict({"name": "s"}) is None


def test_get_client_from_dict_parses() -> None:
    client = get_client_from_dict({"name": "s", "client": "xai:grok-5"})
    assert client == Client.parse("xai:grok-5")


def test_get_client_from_dict_rejects_non_string() -> None:
    with pytest.raises(ValueError, match="must be a string"):
        get_client_from_dict({"name": "s", "client": 5})


def test_get_client_from_dict_rejects_unconvertible_client() -> None:
    with pytest.raises(ValueError, match="must be a string"):
        get_client_from_dict({"name": "s", "client": object()})


def test_get_client_from_dict_ignores_non_json_keys() -> None:
    d: dict[str, object] = {"name": "s", "client": "xai:grok-5", "hook": object()}
    assert get_client_from_dict(d) == Client.parse("xai:grok-5")


def test_child_state_empty_child_id_uses_parent_layout(sandbox) -> None:
    parent_dir = pathlib.Path(sandbox.root) / "artifacts"
    parent_dir.mkdir(parents=True, exist_ok=True)
    state = build_state(data=StateData(), client=FakeClient(), artifact_dir=parent_dir)

    cs = child_state(state, StageAttrs("kid"), fan_out=True, child_id="")

    assert cs.artifact_dir == parent_dir / "kid"
    assert cs.child_key == "kid"
