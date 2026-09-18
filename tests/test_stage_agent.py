"""Unit tests for the Agent primitive stage construction."""

from __future__ import annotations

from typing import Any

import pytest
from _gremlins_core.executor import StateData, build_state
from _gremlins_core.stages import Agent
from conftest import MINIMAL_EVENTS

from tests.fake_client import FakeClient


def _make_agent(
    *,
    prompts: list[str] | None = None,
    interpolation_map: dict[str, str] | None = None,
    bind_map: dict[str, str] | None = None,
    options: dict[str, Any] | None = None,
    name: str = "my-agent",
) -> Agent:
    return Agent(
        name,
        prompts or ["Hello {content}"],
        options or {},
        interpolation_map=interpolation_map,
        bind_map=bind_map,
    )


def test_with_dict_parses_interpolation_and_bind_maps(tmp_path):
    d = {
        "name": "my-agent",
        "type": "agent",
        "prompt": ["Do {task}"],
        "interpolation": {"task": "artifact.task-key"},
        "bind": {"artifact.result": "file://session/result.md"},
    }
    agent = Agent.with_dict(d)
    assert agent.interpolation_map == {"task": "artifact.task-key"}
    assert agent.bind_map == {"artifact.result": "file://session/result.md"}


def test_with_dict_rejects_non_dict_interpolation(tmp_path):
    d = {"name": "x", "type": "agent", "interpolation": "not-a-dict"}
    with pytest.raises(ValueError, match="'interpolation' must be a mapping"):
        Agent.with_dict(d)


def test_with_dict_rejects_non_dict_bind(tmp_path):
    d = {"name": "x", "type": "agent", "bind": ["list"]}
    with pytest.raises(ValueError, match="'bind' must be a mapping"):
        Agent.with_dict(d)


def test_with_dict_rejects_old_in_key(tmp_path):
    d = {"name": "x", "type": "agent", "in": {"task": "key"}}
    with pytest.raises(ValueError, match="'in'/'out' keys are no longer supported"):
        Agent.with_dict(d)


def test_with_dict_rejects_old_out_key(tmp_path):
    d = {"name": "x", "type": "agent", "out": {"key": "uri"}}
    with pytest.raises(ValueError, match="'in'/'out' keys are no longer supported"):
        Agent.with_dict(d)


def test_with_dict_client_explicit_is_true(tmp_path):
    """Agent.with_dict must mark client_explicit=True when a client is present,
    otherwise composite.child_state() falls back to the parent client (#1334)."""
    d = {
        "name": "my-agent",
        "type": "agent",
        "prompt": ["do stuff"],
        "client": "openai:gpt-5",
    }
    agent = Agent.with_dict(d)
    assert agent.client is not None
    assert agent.client_explicit is True


def test_with_dict_no_client_explicit_is_false(tmp_path):
    """When no client key is present, client_explicit must be False."""
    d = {"name": "my-agent", "type": "agent", "prompt": ["do stuff"]}
    agent = Agent.with_dict(d)
    assert agent.client is None
    assert agent.client_explicit is False


def test_child_state_uses_explicit_client(tmp_path):
    """child_state() must select the child's client when client_explicit is set."""
    from _gremlins_core.clients import Client
    from _gremlins_core.schemas import Pipeline
    from _gremlins_core.stages import child_state

    agent = _make_agent(name="child")
    agent.client = Client.parse("xai:grok-5")
    agent.client_explicit = True

    parent_client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    parent_state = build_state(
        data=StateData(),
        client=parent_client,
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
        pipeline_data=Pipeline(
            name="test", path=tmp_path, stages=[Agent("stub", [], {})]
        ),
    )
    child = child_state(parent_state, agent)
    assert child.client is agent.client
    assert child.client is not parent_client


def test_child_state_falls_back_to_parent_when_not_explicit(tmp_path):
    """child_state() must fall back to the parent client when client_explicit is False."""
    from _gremlins_core.clients import Client
    from _gremlins_core.schemas import Pipeline
    from _gremlins_core.stages import child_state

    agent = _make_agent(name="child")
    agent.client = Client.parse("xai:grok-5")
    agent.client_explicit = False

    parent_client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    parent_state = build_state(
        data=StateData(),
        client=parent_client,
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
        pipeline_data=Pipeline(
            name="test", path=tmp_path, stages=[Agent("stub", [], {})]
        ),
    )
    child = child_state(parent_state, agent)
    assert child.client is parent_client
    assert child.client is not agent.client
