"""Tests for StageAttrs defaults and get_client_from_dict."""

from __future__ import annotations

import pytest
from _gremlins_core.clients import Client

from gremlins.stages.composite import StageAttrs, get_client_from_dict


def test_stage_attrs_init_takes_only_name() -> None:
    stage = StageAttrs("my-stage")
    assert stage.name == "my-stage"
    assert stage.client is None
    assert stage.path == ""


def test_path_setter_propagates_to_body() -> None:
    """path setter does not error on leaf stages (no body attribute)."""
    stage = StageAttrs("leaf")
    stage.path = "parent/leaf"
    assert stage.path == "parent/leaf"


def test_deleted_helpers_not_on_stage() -> None:
    stage = StageAttrs("s")
    assert not hasattr(stage, "run_claude")
    assert not hasattr(stage, "bail_command")
    assert not hasattr(stage, "run_subprocess")


def test_get_client_from_dict_absent() -> None:
    assert get_client_from_dict({"name": "s"}) is None


def test_get_client_from_dict_parses() -> None:
    client = get_client_from_dict({"name": "s", "client": "xai:grok-5"})
    assert client == Client.parse("xai:grok-5")


def test_get_client_from_dict_rejects_non_string() -> None:
    with pytest.raises(ValueError, match="must be a string"):
        get_client_from_dict({"name": "s", "client": 5})
