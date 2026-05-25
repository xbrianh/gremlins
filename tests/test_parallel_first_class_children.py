"""Tests for the 'parallel children are first-class gremlins' feature.

Covers:
- FileSessionResolver._path handles file:///absolute/path URIs
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
from gremlins.artifacts.schemes import FileSessionResolver
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.stages.parallel import ParallelStage, _snapshot_registry

# ---------------------------------------------------------------------------
# FileSessionResolver._path: absolute path URIs
# ---------------------------------------------------------------------------


def test_file_resolver_absolute_path_uri(tmp_path: pathlib.Path) -> None:
    """file:///absolute/path URIs resolve directly to that path."""
    target = tmp_path / "some_file.txt"
    target.write_text("hello")

    resolver = FileSessionResolver(tmp_path / "session")
    uri = Uri.parse(f"file://{target}")
    result = resolver._path(uri)
    assert result == target.resolve()


def test_file_resolver_absolute_path_read(tmp_path: pathlib.Path) -> None:
    """read() works for file:///absolute URIs."""
    target = tmp_path / "data.bin"
    target.write_bytes(b"binary content")

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    resolver = FileSessionResolver(session_dir)
    uri = Uri.parse(f"file://{target}")
    assert resolver.read(uri) == b"binary content"


def test_file_resolver_absolute_path_verify_produced(tmp_path: pathlib.Path) -> None:
    """verify_produced() works for file:///absolute URIs."""
    target = tmp_path / "output.txt"
    target.write_text("done")

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    resolver = FileSessionResolver(session_dir)
    uri = Uri.parse(f"file://{target}")
    resolver.verify_produced(uri)  # should not raise

    empty = tmp_path / "empty.txt"
    empty.touch()
    with pytest.raises(FileNotFoundError):
        resolver.verify_produced(Uri.parse(f"file://{empty}"))


def test_file_resolver_session_relative_still_works(tmp_path: pathlib.Path) -> None:
    """Existing file://session/<name> URIs still resolve correctly."""
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    target = session_dir / "output.txt"
    target.write_text("data")

    resolver = FileSessionResolver(session_dir)
    uri = Uri.parse("file://session/output.txt")
    assert resolver._path(uri) == target.resolve()


# ---------------------------------------------------------------------------
# _snapshot_registry: rewriting file://session/ URIs
# ---------------------------------------------------------------------------


def test_snapshot_registry_rewrites_session_uris(tmp_path: pathlib.Path) -> None:
    parent_session = tmp_path / "parent" / "artifacts"
    parent_session.mkdir(parents=True)

    src = tmp_path / "parent" / "registry.json"
    src.write_text(
        json.dumps(
            {
                "my-artifact": "file://session/output.md",
                "other-artifact": "git://ref/HEAD",
                "gh-artifact": "gh://pr/42",
            }
        ),
        encoding="utf-8",
    )

    dst = tmp_path / "child" / "registry.json"
    dst.parent.mkdir(parents=True)

    _snapshot_registry(src, dst, parent_session)

    data = json.loads(dst.read_text(encoding="utf-8"))
    expected_abs = str((parent_session / "output.md").resolve())
    assert data["my-artifact"] == f"file://{expected_abs}"
    # Non-file:// URIs are copied as-is
    assert data["other-artifact"] == "git://ref/HEAD"
    assert data["gh-artifact"] == "gh://pr/42"


def test_snapshot_registry_missing_src_is_noop(tmp_path: pathlib.Path) -> None:
    src = tmp_path / "nonexistent.json"
    dst = tmp_path / "dst.json"
    parent_session = tmp_path / "session"

    _snapshot_registry(src, dst, parent_session)

    assert not dst.exists()


def test_snapshot_registry_empty_src(tmp_path: pathlib.Path) -> None:
    src = tmp_path / "registry.json"
    src.write_text("{}", encoding="utf-8")
    dst = tmp_path / "dst.json"
    parent_session = tmp_path / "session"

    _snapshot_registry(src, dst, parent_session)
    assert json.loads(dst.read_text(encoding="utf-8")) == {}


# ---------------------------------------------------------------------------
# ParallelStage.run: child gets its own state.json under state_root
# ---------------------------------------------------------------------------


def _make_parent_state(sandbox, gremlin_id: str) -> State:
    state_root = paths.state_root()
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    session_dir = state_dir / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "state.json"
    state_file.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")

    data = StateData(gremlin_id=gremlin_id, state_file=state_file)
    return build_state(data=data, client=FakeClaudeClient(), session_dir=session_dir)


def test_parallel_run_creates_child_state_dirs(sandbox) -> None:
    """Each child of a parallel group gets its own directory under state_root."""
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

    assert (state_root / child_id_a / "state.json").exists()
    assert (state_root / child_id_b / "state.json").exists()

    data_a = json.loads((state_root / child_id_a / "state.json").read_text())
    assert data_a["id"] == child_id_a
    assert data_a["parent_id"] == gremlin_id
    assert data_a["group_name"] == "mygroup"
    assert data_a["child_key"] == "child-a"

    data_b = json.loads((state_root / child_id_b / "state.json").read_text())
    assert data_b["id"] == child_id_b
    assert data_b["child_key"] == "child-b"


def test_parallel_run_no_gremlin_id_uses_old_layout(sandbox) -> None:
    """When parent has no gremlin_id, child state lives under parent session_dir/<child>."""
    session_dir = paths.state_root() / "direct" / "some-run" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)

    parent = build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        session_dir=session_dir,
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

    # Old layout: child session_dir = parent.session_dir / child.name
    assert (session_dir / "child-x").is_dir()


# ---------------------------------------------------------------------------
# Child can read parent artifacts via rewritten absolute URIs in registry
# ---------------------------------------------------------------------------


def test_child_reads_parent_artifact_via_registry(sandbox) -> None:
    """After _init_child_dir, child's registry has absolute URIs for parent session files."""
    from gremlins.stages.parallel import _init_child_dir

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
    parent_artifacts = ArtifactRegistry(session_dir=parent_session)
    parent = build_state(
        data=data,
        client=FakeClaudeClient(),
        session_dir=parent_session,
        artifacts=parent_artifacts,
    )

    child_id = f"{gremlin_id}--mygrp--child-z"
    _init_child_dir(parent, child_id, gremlin_id, "mygrp", "child-z")

    child_registry_path = state_root / child_id / "registry.json"
    assert child_registry_path.exists()
    child_reg_data = json.loads(child_registry_path.read_text(encoding="utf-8"))

    # URI must be rewritten to absolute form
    expected_abs = str(artifact_file.resolve())
    assert child_reg_data["result"] == f"file://{expected_abs}"

    # And the child resolver can actually read the file
    child_session = state_root / child_id / "artifacts"
    child_resolver = FileSessionResolver(child_session)
    uri = Uri.parse(child_reg_data["result"])
    content = child_resolver.read(uri)
    assert content == b"parent result"
