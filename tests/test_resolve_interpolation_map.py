"""Tests for resolve_interpolation_map — key lookup, content(), and ? defaults."""

from __future__ import annotations

import asyncio
import pathlib

import pytest
from _gremlins_core.artifacts import (
    ArtifactRegistry,
    MissingArtifact,
    Uri,
    resolve_interpolation_map,
)
from _gremlins_core.stages import Done
from conftest import MINIMAL_EVENTS, MockGremlin

from gremlins.executor.state import StateData, build_state
from gremlins.stages.agent import Agent
from _gremlins_core.stages import Exec
from tests.fake_client import FakeClient


def _make_registry(tmp_path: pathlib.Path) -> ArtifactRegistry:
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return ArtifactRegistry(artifact_dir)


def _make_state(tmp_path: pathlib.Path, client=None):
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=client or FakeClient(),
        artifact_dir=artifact_dir,
        worktree=tmp_path,
    )


# --- resolve_interpolation_map unit tests ---


def test_simple_key_no_dots(tmp_path):
    reg = _make_registry(tmp_path)
    (tmp_path / "artifacts" / "val.txt").write_text("hello")
    reg._set("key", "file://session/val.txt")
    result = resolve_interpolation_map(reg, {"VAR": "key"})
    assert result == {"VAR": "file://session/val.txt"}


def test_unknown_key_raises(tmp_path):
    reg = _make_registry(tmp_path)
    reg._set("pr", "opaque://pr/1")
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"x": "pr.nonexistent"})


def test_empty_trailing_dot_key_raises(tmp_path):
    """Keys with trailing dots are literal — no such artifact exists."""
    reg = _make_registry(tmp_path)
    reg._set("pr", "opaque://pr/1")
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"x": "pr."})


def test_private_like_key_raises_on_missing(tmp_path):
    """Double-underscore keys are literal — no such artifact exists."""
    reg = _make_registry(tmp_path)
    reg._set("pr", "opaque://pr/1")
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"x": "pr.__class__"})


# --- opaque:// opaque URI ---


def test_opaque_uri_key(tmp_path):
    reg = _make_registry(tmp_path)
    reg._set("plan", "opaque://issue/42")
    result = resolve_interpolation_map(reg, {"ref": "plan"})
    assert result == {"ref": "opaque://issue/42"}


def test_non_string_value_coerced_to_str(tmp_path):
    reg = _make_registry(tmp_path)
    reg._set("data", '{"number": 42}')
    result = resolve_interpolation_map(reg, {"ref": "data"})
    assert result == {"ref": '{"number": 42}'}


# --- content() via file artifacts ---


def test_content_resolves_artifact_file(tmp_path):
    reg = _make_registry(tmp_path)
    uri = Uri.parse("artifact://greeting.txt")
    p = pathlib.Path(reg.register(uri))
    p.write_text("hello world", encoding="utf-8")
    result = resolve_interpolation_map(
        reg, {"msg": 'content("artifact://greeting.txt")'}
    )
    assert result == {"msg": "hello world"}


def test_content_with_json_path(tmp_path):
    reg = _make_registry(tmp_path)
    uri = Uri.parse("artifact://pr.json")
    p = pathlib.Path(reg.register(uri))
    p.write_text('{"branch": "feat-x", "number": 7}', encoding="utf-8")
    result = resolve_interpolation_map(
        reg, {"branch": 'content("artifact://pr.json", "branch")'}
    )
    assert result == {"branch": "feat-x"}


def test_content_with_json_path_int(tmp_path):
    reg = _make_registry(tmp_path)
    uri = Uri.parse("artifact://pr.json")
    p = pathlib.Path(reg.register(uri))
    p.write_text('{"number": 42}', encoding="utf-8")
    result = resolve_interpolation_map(
        reg, {"num": 'content("artifact://pr.json", "number")'}
    )
    assert result == {"num": "42"}


def test_content_unknown_key_raises(tmp_path):
    reg = _make_registry(tmp_path)
    uri = Uri.parse("artifact://pr.json")
    p = pathlib.Path(reg.register(uri))
    p.write_text('{"branch": "main"}', encoding="utf-8")
    with pytest.raises((ValueError, KeyError)):
        resolve_interpolation_map(
            reg, {"x": 'content("artifact://pr.json", "nonexistent")'}
        )


def test_content_optional_returns_empty(tmp_path):
    reg = _make_registry(tmp_path)
    result = resolve_interpolation_map(
        reg,
        {"x": 'content("artifact://missing.txt")?'},
    )
    assert result == {"x": ""}


# --- exec integration: content() interpolation ---


def test_exec_content_substitutes_brace_var(tmp_path):
    state = _make_state(tmp_path)
    uri = Uri.parse("artifact://pr.json")
    p = pathlib.Path(state.artifacts.register(uri))
    p.write_text(
        '{"url": "https://github.com/o/r/pull/5", "number": 5, "branch": "my-branch"}',
        encoding="utf-8",
    )

    out_file = tmp_path / "branch.txt"
    stage = Exec(
        "push",
        {"cmds": [f'echo "{{branch}}" > {out_file}']},
        interpolation_map={"branch": 'content("artifact://pr.json", "branch")'},
    )
    gremlin = MockGremlin(state=state)
    result = asyncio.run(stage.run(gremlin))
    assert isinstance(result, Done)
    assert out_file.read_text().strip() == "my-branch"


# --- agent integration: content() interpolation ---


def test_agent_content_substituted_into_prompt(tmp_path):
    client = FakeClient(fixtures={"push-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    uri = Uri.parse("artifact://pr.json")
    p = pathlib.Path(state.artifacts.register(uri))
    p.write_text(
        '{"url": "https://github.com/o/r/pull/9", "number": 9, "branch": "agent-branch"}',
        encoding="utf-8",
    )

    agent = Agent(
        "push-agent",
        ["Push to branch: {branch}"],
        {},
        interpolation_map={"branch": 'content("artifact://pr.json", "branch")'},
    )
    gremlin = MockGremlin(state=state)
    asyncio.run(agent.run(gremlin))

    assert len(client.calls) == 1
    assert "agent-branch" in client.calls[0].prompt
