"""Unit tests for the Agent primitive stage."""

from __future__ import annotations

import asyncio
import json
import pathlib
import re
from typing import TYPE_CHECKING, Any, cast

import pytest
from _gremlins_core.artifacts import ArtifactRegistry, MissingArtifact, Uri
from _gremlins_core.stages import Agent, Bail, Done
from conftest import MINIMAL_EVENTS, MockGremlin

from gremlins.executor.state import State, StateData, build_state
from tests.fake_client import FakeClient

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


def _make_state(
    tmp_path: pathlib.Path,
    client: FakeClient | None = None,
    *,
    registry: ArtifactRegistry | None = None,
    attempt: str = "",
    fixtures: dict | None = None,
) -> State:
    if client is None:
        client = FakeClient(fixtures=fixtures or {"my-agent": MINIMAL_EVENTS})
    reg = registry or ArtifactRegistry(tmp_path / "artifacts")
    data = StateData()
    if attempt:
        state_dir = tmp_path / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        state_file = state_dir / "state.json"
        state_file.write_text(json.dumps({"id": "gr-test", "stage": ""}))
        data.state_file = state_file
        data.patch(attempt=attempt)
    return build_state(
        data=data,
        client=client,
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
        artifacts=reg,
    )


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


# --- in: resolution and prompt interpolation ---


def test_in_content_substituted_into_prompt(tmp_path):
    registry = ArtifactRegistry(tmp_path / "artifacts")
    (tmp_path / "artifacts").mkdir(exist_ok=True)
    registry.write_into_registry(Uri.parse("artifact://plan.md"), "# My Plan")

    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client, registry=registry)
    agent = _make_agent(
        prompts=["Process: {plan_text}"],
        interpolation_map={"plan_text": 'content("artifact://plan.md")'},
    )

    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    assert len(client.calls) == 1
    assert "# My Plan" in client.calls[0].prompt


def test_missing_in_key_raises_missing_artifact(tmp_path):
    state = _make_state(tmp_path)
    agent = _make_agent(interpolation_map={"content": "unbound-key"})

    with pytest.raises(MissingArtifact):
        asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))


def test_no_interpolation_map_runs_prompt_unchanged(tmp_path):
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(prompts=["Static prompt"], interpolation_map=None)

    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    assert isinstance(result, Done)
    assert client.calls[0].prompt.endswith("Static prompt")


# --- out: verification ---


def test_verify_produced_passes_when_output_written(tmp_path):
    class WritingClient(FakeClient):
        async def run(self, prompt, *, label, **kwargs):
            # Extract path from {result} in the prompt.
            m = re.search(r"`([^`]*output\.md)`", prompt)
            assert m, prompt
            pathlib.Path(m.group(1)).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(m.group(1)).write_text("# Output")
            return await super().run(prompt, label=label, **kwargs)

    client = WritingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write output to `{result}`"],
        bind_map={"result": "file://session/output.md"},
    )

    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    assert isinstance(result, Done)
    assert state.artifacts is not None
    assert state.artifacts.is_registered("file://session/output.md")


def test_verify_produced_fails_when_output_missing(tmp_path):
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write output"],
        bind_map={"result": "file://session/missing.md"},
    )

    with pytest.raises(Bail, match="was not produced"):
        asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))


def test_bind_uri_committed_after_agent_produces(tmp_path):
    seen_bound_before_run: list[bool] = []

    class CheckingClient(FakeClient):
        async def run(self, prompt, *, label, **kwargs):
            registry = state.artifacts
            seen_bound_before_run.append(
                registry is not None
                and registry.is_registered("file://session/output.md")
            )
            # Extract path from {result} and write so commit succeeds.
            m = re.search(r"`([^`]*output\.md)`", prompt)
            if m:
                p = pathlib.Path(m.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("# Output", encoding="utf-8")
            return await super().run(prompt, label=label, **kwargs)

    client = CheckingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write output to `{result}`"],
        bind_map={"result": "file://session/output.md"},
    )

    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert seen_bound_before_run == [False]
    assert state.artifacts.is_registered("file://session/output.md")


# --- with_dict parsing ---


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


# --- registry always required ---


# --- single-file out: auto-management ---


def test_single_file_out_prompt_gets_slug_prefixed_name(tmp_path):
    class WritingClient(FakeClient):
        async def run(self, prompt, *, label, cwd=None, **kwargs):
            m = re.search(r"`([^`]*output\.md)`", prompt)
            assert m, prompt
            pathlib.Path(m.group(1)).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(m.group(1)).write_text("# Output", encoding="utf-8")
            return await super().run(prompt, label=label, cwd=cwd, **kwargs)

    client = WritingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write to `{result}`"],
        bind_map={"result": "file://session/output.md"},
    )

    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    assert len(client.calls) == 1
    m = re.search(
        r"Write to `" + re.escape(str(state.artifact_dir)) + r"/output\.md`",
        client.calls[0].prompt,
    )
    assert m, client.calls[0].prompt


def test_single_file_out_keeps_slug_in_artifact_dir(tmp_path):
    """Output file is written at the expected path under artifact_dir."""

    class WritingClient(FakeClient):
        async def run(self, prompt, *, label, cwd=None, **kwargs):
            m = re.search(r"`([^`]*output\.md)`", prompt)
            assert m, prompt
            pathlib.Path(m.group(1)).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(m.group(1)).write_text("# Output", encoding="utf-8")
            return await super().run(prompt, label=label, cwd=cwd, **kwargs)

    client = WritingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write to `{result}`"],
        bind_map={"result": "file://session/output.md"},
    )

    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert isinstance(result, Done)
    output_file = state.artifact_dir / "output.md"
    assert output_file.exists(), f"expected output.md at {output_file}"
    assert output_file.read_text(encoding="utf-8") == "# Output"


def test_single_file_out_uses_substituted_out_map_name(tmp_path):
    class WritingClient(FakeClient):
        async def run(self, prompt, *, label, cwd=None, **kwargs):
            m = re.search(r"`([^`]*review-code\.md)`", prompt)
            assert m, prompt
            pathlib.Path(m.group(1)).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(m.group(1)).write_text("# Review", encoding="utf-8")
            return await super().run(prompt, label=label, cwd=cwd, **kwargs)

    client = WritingClient(fixtures={"review-code": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        name="review-code",
        prompts=["Write review to `{review-code}`"],
        bind_map={"{name}": "file://session/{name}.md"},
    )

    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert isinstance(result, Done)
    output_file = state.artifact_dir / "review-code.md"
    assert output_file.exists()
    assert output_file.read_text(encoding="utf-8") == "# Review"


def test_single_file_out_missing_source_raises(tmp_path):
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write nothing"],
        bind_map={"result": "file://session/never-written.md"},
    )

    with pytest.raises(Bail, match="was not produced"):
        asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))


# --- multi-file out: auto-management ---


def test_multi_file_out_prompt_gets_per_key_paths(tmp_path):
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write to {blah}, {foo}, {biz}"],
        bind_map={
            "blah?": "file://session/blah.md",
            "foo?": "file://session/foo.md",
            "biz?": "file://session/biz.md",
        },
    )

    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    assert len(client.calls) == 1
    prompt = client.calls[0].prompt
    ad = str(state.artifact_dir)
    for key in ("blah", "foo", "biz"):
        assert re.search(rf"{re.escape(ad)}/{re.escape(key)}\.md", prompt), (
            f"{key} not found in prompt"
        )


def test_multi_file_out_renames_only_written_files(tmp_path):
    class WritingClient(FakeClient):
        async def run(self, prompt, *, label, cwd=None, **kwargs):
            # Write two of the three declared files; biz.md is skipped.
            for key in ("blah", "foo"):
                m = re.search(rf"({re.escape(key)}\.md)", prompt)
                if m:
                    p = state.artifact_dir / m.group(1)
                    p.parent.mkdir(parents=True, exist_ok=True)
                    p.write_text(f"# {key}.md", encoding="utf-8")
            return await super().run(prompt, label=label, cwd=cwd, **kwargs)

    client = WritingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write to {blah} and {foo}"],
        bind_map={
            "blah": "file://session/blah.md",
            "foo": "file://session/foo.md",
            "biz?": "file://session/biz.md",
        },
    )

    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert isinstance(result, Done)
    # Files are written directly at the expected paths.
    blah_file = state.artifact_dir / "blah.md"
    foo_file = state.artifact_dir / "foo.md"
    assert blah_file.exists()
    assert foo_file.exists()
    assert blah_file.read_text(encoding="utf-8") == "# blah.md"
    assert foo_file.read_text(encoding="utf-8") == "# foo.md"
    assert not (state.artifact_dir / "biz.md").exists()


# --- client_explicit regression ---


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
    from _gremlins_core.schemas import Pipeline

    from gremlins.stages.composite import child_state

    agent = _make_agent(name="child")
    from _gremlins_core.clients import RustClient as Client

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
    from _gremlins_core.clients import RustClient as Client
    from _gremlins_core.schemas import Pipeline

    from gremlins.stages.composite import child_state

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


def test_multi_file_out_missing_optional_files_do_not_raise(tmp_path):
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write nothing"],
        bind_map={
            "blah?": "file://session/blah.md",
            "foo?": "file://session/foo.md",
        },
    )

    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert isinstance(result, Done)


def test_multi_file_out_missing_non_optional_raises(tmp_path):
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    agent = _make_agent(
        prompts=["Write nothing"],
        bind_map={
            "blah": "file://session/blah.md",
            "foo": "file://session/foo.md",
        },
    )

    with pytest.raises(Bail, match="was not produced"):
        asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))


# ---------------------------------------------------------------------------
# loop_iter in bind URIs and interpolation values
# ---------------------------------------------------------------------------


def test_loop_iter_in_bind_uri(tmp_path):
    """{loop_iter} in bind URIs is resolved before registration."""

    class WritingClient(FakeClient):
        async def run(self, prompt, *, label, cwd=None, **kwargs):
            import re

            m = re.search(r"`([^`]*out\.txt)`", prompt)
            if m:
                p = pathlib.Path(m.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("data")
            return await super().run(prompt, label=label, cwd=cwd, **kwargs)

    client = WritingClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    state.loop_stack = [("my-agent", 3)]
    agent = _make_agent(
        prompts=["Write to `{out}`"],
        bind_map={"out": "artifact://{loop_iter}/out.txt"},
    )
    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert isinstance(result, Done)
    assert state.artifacts.is_registered("artifact://my-agent~3/out.txt")


def test_loop_iter_in_interpolation_value(tmp_path):
    """{loop_iter} in interpolation values is resolved before resolution."""
    client = FakeClient(fixtures={"my-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    state.loop_stack = [("my-agent", 2)]
    state.artifacts.write_into_registry(
        Uri.parse("artifact://my-agent~2/plan.md"), "# Plan for iteration 2"
    )
    agent = _make_agent(
        prompts=["Plan: {plan}"],
        interpolation_map={"plan": 'content("artifact://{loop_iter}/plan.md")'},
    )
    result = asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert isinstance(result, Done)
    assert "# Plan for iteration 2" in client.calls[0].prompt


# ---------------------------------------------------------------------------
# End-to-end tests absorbed from test_stage_agent_helper.py
# ---------------------------------------------------------------------------


def test_calls_client_run_with_expected_kwargs(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    agent = _make_agent(prompts=["hello"])

    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "my-agent"
    assert "hello" in call.prompt
    assert call.model == state.client.model


def test_model_kwarg_forwarded(tmp_path):
    state = _make_state(tmp_path)
    agent = _make_agent(prompts=["hello"], options={"model": "haiku"})

    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    assert state.client.calls[0].model == "haiku"


def test_token_usage_accumulated_into_state(tmp_path):
    usage_events = [
        {
            "type": "result",
            "result": "done",
            "token_usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cached_input_tokens": 90,
                "cache_creation_input_tokens": 5,
                "reasoning_tokens": 8,
                "turns": 3,
            },
        }
    ]
    state = _make_state(tmp_path, attempt="att1", fixtures={"my-agent": usage_events})
    agent = _make_agent(prompts=["hello"])
    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))
    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    data = json.loads(state.data.state_file.read_text())
    assert data["token_usage"] == {
        "prompt_tokens": 200,
        "completion_tokens": 40,
        "cached_input_tokens": 180,
        "cache_creation_input_tokens": 10,
        "reasoning_tokens": 16,
        "turns": 6,
    }


def test_token_usage_absent_is_noop(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    agent = _make_agent(prompts=["hello"])
    asyncio.run(agent.run(cast("Gremlin", MockGremlin(state))))

    data = json.loads(state.data.state_file.read_text())
    assert "token_usage" not in data
