"""Tests for the 'parallel children are first-class gremlins' feature.

from conftest import _TestGremlin
Covers:
- FileArtifactResolver._path handles file:///absolute/path URIs
- _snapshot_registry rewrites file://session/ URIs to absolute paths
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

    asyncio.run(stage.run(_TestGremlin(parent)))

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
    asyncio.run(stage.run(_TestGremlin(parent)))

    # Old layout: child artifact_dir = parent.artifact_dir / child.name
    assert (artifact_dir / "child-x").is_dir()


# ---------------------------------------------------------------------------
# Child artifact dir is a full copy of parent artifacts via fork()
# ---------------------------------------------------------------------------


def test_parallel_child_artifact_dir_is_full_copy(sandbox) -> None:
    """Child's artifact dir contains a full copy of parent's artifacts."""
    import subprocess

    from gremlins.executor.gremlin import Gremlin
    from gremlins.pipeline import Pipeline

    # Create a temporary git repo
    tmp_repo = sandbox.root / "repo"
    tmp_repo.mkdir()
    subprocess.run(_TestGremlin(["git", "init"], cwd=tmp_repo, check=True, capture_output=True))
    subprocess.run(
        ["git", "config", "user.email", "test@example.com"],
        cwd=tmp_repo,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test User"],
        cwd=tmp_repo,
        check=True,
        capture_output=True,
    )
    (tmp_repo / "file.txt").write_text("initial")
    subprocess.run(
        ["git", "add", "file.txt"], cwd=tmp_repo, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "initial"],
        cwd=tmp_repo,
        check=True,
        capture_output=True,
    )

    gremlin_id = "parent-fork-test"
    state_root = paths.state_root()
    parent_state_dir = state_root / gremlin_id
    parent_state_dir.mkdir(parents=True, exist_ok=True)
    parent_artifact_dir = parent_state_dir / "artifacts"
    parent_artifact_dir.mkdir(parents=True, exist_ok=True)

    # Create parent artifacts
    (parent_artifact_dir / "file1.txt").write_text("content1")
    (parent_artifact_dir / "file2.txt").write_text("content2")
    (parent_artifact_dir / "subdir").mkdir()
    (parent_artifact_dir / "subdir" / "file3.txt").write_text("content3")

    # Set up parent registry
    parent_registry = parent_state_dir / "registry.json"
    parent_registry.write_text(
        json.dumps(
            {
                "artifact1": "file://session/file1.txt",
                "artifact2": "file://session/file2.txt",
            }
        ),
        encoding="utf-8",
    )

    state_file = parent_state_dir / "state.json"
    state_file.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")
    data = StateData(gremlin_id=gremlin_id, state_file=state_file)
    parent_artifacts = ArtifactRegistry(artifact_dir=parent_artifact_dir)
    parent_artifacts.bind("artifact1", Uri.parse("file://session/file1.txt"))
    parent_artifacts.bind("artifact2", Uri.parse("file://session/file2.txt"))

    parent = build_state(
        data=data,
        client=FakeClaudeClient(),
        artifact_dir=parent_artifact_dir,
        artifacts=parent_artifacts,
        cwd=str(tmp_repo),
        worktree=tmp_repo,
    )

    # Fork the parent state
    gremlin = Gremlin(
        stages=[],
        state_dir=parent_state_dir,
        gremlin_id=gremlin_id,
        pipeline_data=Pipeline(name="test", path=tmp_repo, stages=[]),
        project_root=str(tmp_repo),
    )
    gremlin.registry = parent_artifacts

    child_id = f"{gremlin_id}--mygrp--child-z"

    async def test_fork():
        return await gremlin.fork(
            parent,
            child_id,
            parent_id=gremlin_id,
            group_name="mygrp",
            child_key="child-z",
        )

    forked = asyncio.run(_TestGremlin(test_fork()))

    # Verify child artifact dir is a full copy
    child_artifact_dir = forked.artifact_dir
    assert (child_artifact_dir / "file1.txt").read_text() == "content1"
    assert (child_artifact_dir / "file2.txt").read_text() == "content2"
    assert (child_artifact_dir / "subdir" / "file3.txt").read_text() == "content3"

    # Verify child registry is copied verbatim
    child_registry_path = state_root / child_id / "registry.json"
    assert child_registry_path.exists()
    child_reg_data = json.loads(child_registry_path.read_text(encoding="utf-8"))
    assert child_reg_data["artifact1"] == "file://session/file1.txt"
    assert child_reg_data["artifact2"] == "file://session/file2.txt"

    # Cleanup
    if forked.worktree and forked.worktree.exists():
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(forked.worktree)],
            cwd=tmp_repo,
            capture_output=True,
        )
