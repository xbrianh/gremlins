"""Tests for the 'parallel children are first-class gremlins' feature.

Covers:
- FileArtifactResolver._path handles file:///absolute/path URIs
- fork_state copies artifact directory and registry verbatim
- ParallelStage.run creates <state_root>/<child_id>/state.json for each child
- A child can read a parent-bound file://session/ artifact via the inherited registry
"""

from __future__ import annotations

import asyncio
import json
import pathlib

import pytest

from gremlins import paths
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.schemes import FileArtifactResolver
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.stages.parallel import ParallelStage

# ---------------------------------------------------------------------------
# FileArtifactResolver._path: absolute path URIs
# ---------------------------------------------------------------------------


def test_file_resolver_absolute_path_uri(tmp_path: pathlib.Path) -> None:
    """file:///absolute/path URIs resolve directly to that path."""
    target = tmp_path / "some_file.txt"
    target.write_text("hello")

    resolver = FileArtifactResolver(tmp_path / "session")
    uri = Uri.parse(f"file://{target}")
    result = resolver._path(uri)
    assert result == target.resolve()


def test_file_resolver_absolute_path_read(tmp_path: pathlib.Path) -> None:
    """read() works for file:///absolute URIs."""
    target = tmp_path / "data.bin"
    target.write_bytes(b"binary content")

    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    resolver = FileArtifactResolver(artifact_dir)
    uri = Uri.parse(f"file://{target}")
    assert resolver.read(uri) == "binary content"


def test_file_resolver_absolute_path_verify_produced(tmp_path: pathlib.Path) -> None:
    """verify_produced() works for file:///absolute URIs."""
    target = tmp_path / "output.txt"
    target.write_text("done")

    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    resolver = FileArtifactResolver(artifact_dir)
    uri = Uri.parse(f"file://{target}")
    resolver.verify_produced(uri)  # should not raise

    empty = tmp_path / "empty.txt"
    empty.touch()
    with pytest.raises(FileNotFoundError):
        resolver.verify_produced(Uri.parse(f"file://{empty}"))


def test_file_resolver_session_relative_still_works(tmp_path: pathlib.Path) -> None:
    """Existing file://session/<name> URIs still resolve correctly."""
    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    target = artifact_dir / "output.txt"
    target.write_text("data")

    resolver = FileArtifactResolver(artifact_dir)
    uri = Uri.parse("file://session/output.txt")
    assert resolver._path(uri) == target.resolve()


# ---------------------------------------------------------------------------
# ParallelStage.run: child gets its own state.json under state_root
# ---------------------------------------------------------------------------


def _make_parent_state(sandbox, gremlin_id: str) -> State:
    state_root = paths.state_root()
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = state_dir / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "state.json"
    state_file.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")

    data = StateData(gremlin_id=gremlin_id, state_file=state_file)
    return build_state(data=data, client=FakeClaudeClient(), artifact_dir=artifact_dir)


def test_parallel_run_cleans_up_child_state_dirs(sandbox) -> None:
    """Child state dirs are removed after a successful parallel run."""
    gremlin_id = "parent-gremlin-abc"
    parent = _make_parent_state(sandbox, gremlin_id)

    from gremlins.stages.base import Stage
    from gremlins.stages.outcome import Done, Outcome

    class _NoopStage(Stage):
        type = "_test_noop_v2"

        async def run(self, state: State) -> Outcome:
            return Done()

    child_a = _NoopStage("child-a")
    child_b = _NoopStage("child-b")
    stage = ParallelStage("mygroup", [child_a, child_b])

    asyncio.run(stage.run(parent))

    state_root = paths.state_root()
    child_id_a = f"{gremlin_id}--mygroup--child-a"
    child_id_b = f"{gremlin_id}--mygroup--child-b"

    assert not (state_root / child_id_a).exists()
    assert not (state_root / child_id_b).exists()


def test_parallel_run_no_gremlin_id_uses_old_layout(sandbox) -> None:
    """When parent has no gremlin_id, child state lives under parent artifact_dir/<child>."""
    artifact_dir = paths.state_root() / "direct" / "some-run" / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)

    parent = build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        artifact_dir=artifact_dir,
    )

    from gremlins.stages.base import Stage
    from gremlins.stages.outcome import Done, Outcome

    class _NoopStage(Stage):
        type = "_test_noop_v3"

        async def run(self, state: State) -> Outcome:
            return Done()

    child = _NoopStage("child-x")
    stage = ParallelStage("grp", [child])
    asyncio.run(stage.run(parent))

    # Old layout: child artifact_dir = parent.artifact_dir / child.name
    assert (artifact_dir / "child-x").is_dir()


# ---------------------------------------------------------------------------
# Child can read parent artifacts via rewritten absolute URIs in registry
# ---------------------------------------------------------------------------


def test_child_reads_parent_artifact_via_fork(sandbox) -> None:
    """After fork_state, child can read parent artifacts via copied artifact dir."""

    async def _test():
        from gremlins.executor.fork import fork_state

        gremlin_id = "parent-reg-test"
        state_root = paths.state_root()
        parent_state_dir = state_root / gremlin_id
        parent_state_dir.mkdir(parents=True, exist_ok=True)
        parent_session = parent_state_dir / "artifacts"
        parent_session.mkdir(parents=True, exist_ok=True)

        # Write a file the parent "produced" into its session dir
        artifact_file = parent_session / "result.md"
        artifact_file.write_text("parent result")

        # Set up parent registry pointing at it via file://session/ URI
        parent_registry = parent_state_dir / "registry.json"
        parent_registry.write_text(
            json.dumps({"result": "file://session/result.md"}), encoding="utf-8"
        )

        state_file = parent_state_dir / "state.json"
        state_file.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")
        data = StateData(gremlin_id=gremlin_id, state_file=state_file)
        parent_artifacts = ArtifactRegistry(artifact_dir=parent_session)
        parent = build_state(
            data=data,
            client=FakeClaudeClient(),
            artifact_dir=parent_session,
            artifacts=parent_artifacts,
        )

        child_id = f"{gremlin_id}--mygrp--child-z"
        await fork_state(
            parent,
            child_id,
            project_root=".",
            state_root=state_root,
            parent_id=gremlin_id,
            group_name="mygrp",
            child_key="child-z",
        )

        # Child registry is copied verbatim
        child_registry_path = state_root / child_id / "registry.json"
        assert child_registry_path.exists()
        child_reg_data = json.loads(child_registry_path.read_text(encoding="utf-8"))

        # URI is still file://session/ because artifacts are copied
        assert child_reg_data["result"] == "file://session/result.md"

        # And the child resolver can actually read the file via the copied artifact dir
        child_session = state_root / child_id / "artifacts"
        child_resolver = FileArtifactResolver(child_session)
        uri = Uri.parse(child_reg_data["result"])
        content = child_resolver.read(uri)
        assert content == "parent result"

    asyncio.run(_test())
