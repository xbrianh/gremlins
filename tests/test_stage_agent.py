"""Unit tests for the Agent primitive stage."""

from __future__ import annotations

import asyncio
import pathlib

import pytest

from conftest import MINIMAL_EVENTS

from gremlins.artifacts.registry import ArtifactRegistry, MissingArtifact
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData
from gremlins.stages.agent import Agent
from gremlins.stages.outcome import Done


def _make_state(
    tmp_path: pathlib.Path,
    client: FakeClaudeClient | None = None,
    *,
    registry: ArtifactRegistry | None = None,
) -> State:
    if client is None:
        client = FakeClaudeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    reg = registry or ArtifactRegistry(tmp_path, cwd=tmp_path)
    return State(
        data=StateData(),
        client=client,
        session_dir=tmp_path,
        worktree=tmp_path,
        artifacts=reg,
    )


def _make_agent(
    *,
    prompts: list[str] | None = None,
    in_map: dict[str, str] | None = None,
    out_map: dict[str, str] | None = None,
    name: str = "my-agent",
) -> Agent:
    return Agent(
        name,
        prompts or ["Hello {content}"],
        {},
        in_map=in_map,
        out_map=out_map,
    )


# --- in: resolution and prompt interpolation ---


def test_in_content_substituted_into_prompt(tmp_path):
    registry = ArtifactRegistry(tmp_path, cwd=tmp_path)
    (tmp_path / "plan.md").write_bytes(b"# My Plan")
    registry.bind("plan", Uri.parse("file://session/plan.md"))

    client = FakeClaudeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client, registry=registry)
    agent = _make_agent(prompts=["Process: {plan_text}"], in_map={"plan_text": "plan"})

    asyncio.run(agent.run(state))

    assert len(client.calls) == 1
    assert "# My Plan" in client.calls[0].prompt


def test_missing_in_key_raises_missing_artifact(tmp_path):
    state = _make_state(tmp_path)
    agent = _make_agent(in_map={"content": "unbound-key"})

    with pytest.raises(MissingArtifact):
        asyncio.run(agent.run(state))


def test_no_in_map_runs_prompt_unchanged(tmp_path):
    client = FakeClaudeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(prompts=["Static prompt"], in_map=None)

    result = asyncio.run(agent.run(state))

    assert isinstance(result, Done)
    assert client.calls[0].prompt == "Static prompt"


# --- out: verification ---


def test_verify_produced_passes_when_out_file_written(tmp_path):
    output_file = tmp_path / "output.md"

    class WritingClient(FakeClaudeClient):
        async def run(self, prompt, *, label, **kwargs):
            output_file.write_text("# Output")
            return await super().run(prompt, label=label, **kwargs)

    client = WritingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write output"],
        out_map={"result": "file://session/output.md"},
    )

    result = asyncio.run(agent.run(state))

    assert isinstance(result, Done)
    assert state.artifacts is not None
    assert state.artifacts.produced("result")


def test_verify_produced_fails_when_out_file_missing(tmp_path):
    client = FakeClaudeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write output"],
        out_map={"result": "file://session/missing.md"},
    )

    with pytest.raises(FileNotFoundError):
        asyncio.run(agent.run(state))


def test_out_uri_bound_in_registry_before_agent_runs(tmp_path):
    output_file = tmp_path / "output.md"
    seen_bound_before_run: list[bool] = []

    class CheckingClient(FakeClaudeClient):
        async def run(self, prompt, *, label, **kwargs):
            # Check that the out: key is bound before the agent runs
            registry = state.artifacts
            seen_bound_before_run.append(
                registry is not None and registry.produced("result")
            )
            output_file.write_text("# Output")
            return await super().run(prompt, label=label, **kwargs)

    client = CheckingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        out_map={"result": "file://session/output.md"},
    )

    asyncio.run(agent.run(state))
    assert seen_bound_before_run == [True]


# --- with_dict parsing ---


def test_with_dict_parses_in_and_out_maps(tmp_path):
    d = {
        "name": "my-agent",
        "type": "agent",
        "prompt": ["Do {task}"],
        "in": {"task": "task-key"},
        "out": {"result": "file://session/result.md"},
    }
    agent = Agent.with_dict(d)
    assert agent.in_map == {"task": "task-key"}
    assert agent.out_map == {"result": "file://session/result.md"}


def test_with_dict_rejects_non_dict_in(tmp_path):
    d = {"name": "x", "type": "agent", "in": "not-a-dict"}
    with pytest.raises(ValueError, match="'in' must be a mapping"):
        Agent.with_dict(d)


def test_with_dict_rejects_non_dict_out(tmp_path):
    d = {"name": "x", "type": "agent", "out": ["list"]}
    with pytest.raises(ValueError, match="'out' must be a mapping"):
        Agent.with_dict(d)


# --- fallback registry when state.artifacts is None ---


def test_fallback_registry_created_when_state_artifacts_none(tmp_path):
    client = FakeClaudeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = State(
        data=StateData(),
        client=client,
        session_dir=tmp_path,
        worktree=tmp_path,
        artifacts=None,
    )
    agent = _make_agent(prompts=["Static"], in_map=None)

    result = asyncio.run(agent.run(state))
    assert isinstance(result, Done)
