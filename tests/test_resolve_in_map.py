"""Tests for dotted-key resolution in resolve_in_map."""

from __future__ import annotations

import asyncio
import pathlib

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.resolve import resolve_in_map
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.stages.agent import Agent
from gremlins.stages.exec import Exec
from gremlins.stages.outcome import Done


def _make_registry(tmp_path: pathlib.Path) -> ArtifactRegistry:
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return ArtifactRegistry(artifact_dir, cwd=tmp_path)


def _make_state(tmp_path: pathlib.Path, client=None):
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=client or FakeClaudeClient(),
        artifact_dir=artifact_dir,
        worktree=tmp_path,
    )


# --- resolve_in_map unit tests ---


def test_simple_key_no_dots(tmp_path):
    reg = _make_registry(tmp_path)
    (tmp_path / "artifacts" / "val.txt").write_text("hello")
    reg.bind("key", Uri.parse("file://session/val.txt"))
    result = resolve_in_map(reg, {"VAR": "key"})
    assert result == {"VAR": "hello"}


def test_dotted_key_reads_attribute(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write(
        "pr", {"url": "https://github.com/o/r/pull/7", "number": 7, "branch": "feat-x"}
    )
    result = resolve_in_map(reg, {"branch": "pr.branch"})
    assert result == {"branch": "feat-x"}


def test_dotted_key_number_attribute(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write(
        "pr", {"url": "https://github.com/o/r/pull/42", "number": 42, "branch": "main"}
    )
    result = resolve_in_map(reg, {"num": "pr.number"})
    assert result == {"num": "42"}


def test_dotted_key_url_attribute(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write(
        "pr", {"url": "https://github.com/o/r/pull/3", "number": 3, "branch": "fix"}
    )
    result = resolve_in_map(reg, {"url": "pr.url"})
    assert result == {"url": "https://github.com/o/r/pull/3"}


def test_nested_dotted_path(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write("obj", {"inner": {"value": "deep"}})
    result = resolve_in_map(reg, {"v": "obj.inner.value"})
    assert result == {"v": "deep"}


def test_unknown_attribute_raises(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write(
        "pr", {"url": "https://github.com/o/r/pull/1", "number": 1, "branch": "b"}
    )
    with pytest.raises(ValueError, match="has no key"):
        resolve_in_map(reg, {"x": "pr.nonexistent"})


def test_private_attribute_raises(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write(
        "pr", {"url": "https://github.com/o/r/pull/1", "number": 1, "branch": "b"}
    )
    with pytest.raises(ValueError, match="private attribute"):
        resolve_in_map(reg, {"x": "pr.__class__"})


def test_empty_segment_raises(tmp_path):
    reg = _make_registry(tmp_path)
    with pytest.raises(ValueError, match="empty segment"):
        resolve_in_map(reg, {"x": "pr."})


# --- gh:// opaque URI returns {"uri": ...} ---


def test_gh_opaque_uri_attribute(tmp_path):
    reg = _make_registry(tmp_path)
    reg.bind("plan", Uri.parse("gh://issue/42"))
    result = resolve_in_map(reg, {"ref": "plan.uri"})
    assert result == {"ref": "gh://issue/42"}


# --- exec integration: dotted key becomes env var ---


def test_exec_dotted_key_injects_env_var(tmp_path):
    state = _make_state(tmp_path)
    state.artifacts.write(
        "pr",
        {"url": "https://github.com/o/r/pull/5", "number": 5, "branch": "my-branch"},
    )

    out_file = tmp_path / "branch.txt"
    stage = Exec(
        "push",
        {"cmds": [f'echo "$branch" > {out_file}']},
        in_map={"branch": "pr.branch"},
    )
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    assert out_file.read_text().strip() == "my-branch"


# --- agent integration: dotted key substituted into prompt ---


def test_agent_dotted_key_substituted_into_prompt(tmp_path):
    client = FakeClaudeClient(fixtures={"push-agent": MINIMAL_EVENTS})
    state = _make_state(tmp_path, client)
    state.artifacts.write(
        "pr",
        {"url": "https://github.com/o/r/pull/9", "number": 9, "branch": "agent-branch"},
    )

    agent = Agent(
        "push-agent",
        ["Push to branch: {branch}"],
        {},
        in_map={"branch": "pr.branch"},
    )
    asyncio.run(agent.run(state))

    assert len(client.calls) == 1
    assert "agent-branch" in client.calls[0].prompt
