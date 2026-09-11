"""Tests for the 'parallel children are first-class gremlins' feature.

Covers:
- _snapshot_registry rewrites file://session/ URIs to absolute paths
- ParallelStage.run creates <state_root>/<child_id>/state.json for each child
- A child can read a parent-bound file://session/ artifact via the inherited registry
"""

from __future__ import annotations

import asyncio
import json
import pathlib

from _gremlins_core.artifacts import ArtifactRegistry, Uri
from _gremlins_core.config import scratch_root
from _gremlins_core.config import state_root as _state_root_func
from conftest import MockGremlin

from gremlins.executor.state import State, StateData, build_state
from gremlins.stages.parallel import ParallelStage
from tests.fake_client import FakeClient

# ---------------------------------------------------------------------------
# ParallelStage.run: child gets its own state.json under state_root
# ---------------------------------------------------------------------------


def _make_parent_state(sandbox, gremlin_id: str) -> State:
    state_root = pathlib.Path(_state_root_func())
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    artifact_dir = pathlib.Path(scratch_root(gremlin_id)) / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "state.json"
    state_file.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")

    data = StateData(gremlin_id=gremlin_id)
    data.state_file = state_file
    return build_state(data=data, client=FakeClient(), artifact_dir=artifact_dir)


def test_parallel_run_cleans_up_child_state_dirs(sandbox) -> None:
    """Child state dirs are removed after a successful parallel run."""
    gremlin_id = "parent-gremlin-abc"
    parent = _make_parent_state(sandbox, gremlin_id)

    from _gremlins_core.stages import Done, Outcome

    from gremlins.stages.composite import StageAttrs

    class _NoopStage(StageAttrs):
        type = "_test_noop_v2"

        async def run(self, gremlin) -> Outcome:
            return Done()

    child_a = _NoopStage("child-a")
    child_b = _NoopStage("child-b")
    stage = ParallelStage("mygroup", [child_a, child_b])

    gremlin = MockGremlin(state=parent)
    asyncio.run(stage.run(gremlin))

    state_root = pathlib.Path(_state_root_func())
    child_id_a = f"{gremlin_id}--mygroup--child-a"
    child_id_b = f"{gremlin_id}--mygroup--child-b"

    assert not (state_root / child_id_a).exists()
    assert not (state_root / child_id_b).exists()


def test_parallel_run_no_gremlin_id_uses_old_layout(sandbox) -> None:
    """When parent has no gremlin_id, child state lives under parent artifact_dir/<child>."""
    artifact_dir = pathlib.Path(scratch_root(None)) / "some-run" / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)

    parent = build_state(
        data=StateData(),
        client=FakeClient(),
        artifact_dir=artifact_dir,
    )

    from _gremlins_core.stages import Done, Outcome

    from gremlins.stages.composite import StageAttrs

    class _NoopStage(StageAttrs):
        type = "_test_noop_v3"

        async def run(self, gremlin) -> Outcome:
            return Done()

    child = _NoopStage("child-x")
    stage = ParallelStage("grp", [child])
    gremlin = MockGremlin(state=parent)
    asyncio.run(stage.run(gremlin))

    # Old layout: child artifact_dir = parent.artifact_dir / child.name
    assert (artifact_dir / "child-x").is_dir()


# ---------------------------------------------------------------------------
# Child artifact dir is a full copy of parent artifacts via fork()
# ---------------------------------------------------------------------------


def test_parallel_child_artifact_dir_is_full_copy(sandbox) -> None:
    """Child's artifact dir contains a full copy of parent's artifacts."""
    import subprocess

    from _gremlins_core.schemas import Pipeline

    from gremlins.executor.gremlin import Gremlin

    # Create a temporary git repo
    tmp_repo = sandbox.root / "repo"
    tmp_repo.mkdir()
    subprocess.run(["git", "init"], cwd=tmp_repo, check=True, capture_output=True)
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
    state_root = pathlib.Path(_state_root_func())
    parent_state_dir = state_root / gremlin_id
    parent_state_dir.mkdir(parents=True, exist_ok=True)
    parent_artifact_dir = pathlib.Path(scratch_root(gremlin_id)) / "artifacts"
    parent_artifact_dir.mkdir(parents=True, exist_ok=True)

    # Create parent artifacts
    (parent_artifact_dir / "file1.txt").write_text("content1")
    (parent_artifact_dir / "file2.txt").write_text("content2")
    (parent_artifact_dir / "subdir").mkdir()
    (parent_artifact_dir / "subdir" / "file3.txt").write_text("content3")

    state_file = parent_state_dir / "state.json"
    state_file.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")
    data = StateData(gremlin_id=gremlin_id)
    data.state_file = state_file
    parent_artifacts = ArtifactRegistry(artifact_dir=parent_artifact_dir)
    parent_artifacts.write_into_registry(Uri.parse("artifact://file1.txt"), "content1")
    parent_artifacts.write_into_registry(Uri.parse("artifact://file2.txt"), "content2")

    parent = build_state(
        data=data,
        client=FakeClient(),
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

    forked = asyncio.run(test_fork())

    # Verify child artifact dir is a full copy
    child_artifact_dir = forked.artifact_dir
    assert (child_artifact_dir / "file1.txt").read_text() == "content1"
    assert (child_artifact_dir / "file2.txt").read_text() == "content2"
    assert (child_artifact_dir / "subdir" / "file3.txt").read_text() == "content3"

    # Verify child registry is copied verbatim
    child_registry_path = pathlib.Path(scratch_root(child_id)) / "registry.json"
    assert child_registry_path.exists()
    child_reg_data = json.loads(child_registry_path.read_text(encoding="utf-8"))
    assert "artifact://file1.txt" in child_reg_data
    assert "artifact://file2.txt" in child_reg_data

    # Cleanup
    if forked.worktree and forked.worktree.exists():
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(forked.worktree)],
            cwd=tmp_repo,
            capture_output=True,
        )


def test_fork_uses_parent_not_child_state_as_source(sandbox) -> None:
    """Regression: fork() copies artifacts from the parent gremlin (self),
    not from the child state's artifact_dir (which is empty when created via
    child_state(fan_out=True) as _ParallelExecutor._fan_out does)."""
    import subprocess

    from _gremlins_core.schemas import Pipeline

    from gremlins.executor.gremlin import Gremlin
    from gremlins.stages.composite import StageAttrs, child_state

    # Create a temporary git repo
    tmp_repo = sandbox.root / "repo"
    tmp_repo.mkdir()
    subprocess.run(["git", "init"], cwd=tmp_repo, check=True, capture_output=True)
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

    gremlin_id = "parent-regression"
    state_root = pathlib.Path(_state_root_func())
    parent_state_dir = state_root / gremlin_id
    parent_state_dir.mkdir(parents=True, exist_ok=True)
    parent_artifact_dir = pathlib.Path(scratch_root(gremlin_id)) / "artifacts"
    parent_artifact_dir.mkdir(parents=True, exist_ok=True)

    # Create parent artifacts
    (parent_artifact_dir / "plan.md").write_text("# Plan\nSome plan content")
    (parent_artifact_dir / "spec.md").write_text("# Spec\nSome spec")

    # Persist parent registry.json (simulates what a real run does)
    parent_registry_path = parent_state_dir / "registry.json"
    parent_registry_path.write_text(
        json.dumps(
            {
                "plan": "file://session/plan.md",
                "spec": "file://session/spec.md",
            }
        ),
        encoding="utf-8",
    )

    state_file = parent_state_dir / "state.json"
    state_file.write_text(
        json.dumps(
            {"id": gremlin_id, "client": "cmd:fake", "project_root": str(tmp_repo)}
        ),
        encoding="utf-8",
    )
    data = StateData(gremlin_id=gremlin_id)
    data.state_file = state_file
    parent_artifacts = ArtifactRegistry(artifact_dir=parent_artifact_dir)
    parent_artifacts.write_into_registry(
        Uri.parse("artifact://plan.md"), "# Plan\nSome plan content"
    )
    parent_artifacts.write_into_registry(
        Uri.parse("artifact://spec.md"), "# Spec\nSome spec"
    )

    parent_state = build_state(
        data=data,
        client=FakeClient(),
        artifact_dir=parent_artifact_dir,
        artifacts=parent_artifacts,
        cwd=str(tmp_repo),
        worktree=tmp_repo,
    )

    # Create the parent gremlin
    gremlin = Gremlin(
        stages=[],
        state_dir=parent_state_dir,
        gremlin_id=gremlin_id,
        pipeline_data=Pipeline(name="test", path=tmp_repo, stages=[]),
        project_root=str(tmp_repo),
    )
    gremlin.registry = parent_artifacts

    # Simulate what _ParallelExecutor._fan_out does:
    # 1. Create a child state via child_state(fan_out=True) — this gives artifact_dir
    #    pointing to an empty directory under scratch_root(<child_id>)/artifacts/
    child_stage = StageAttrs("child-x")
    child_stage.type = "agent"
    child_id = f"{gremlin_id}--mygroup--child-x"
    cs = child_state(parent_state, child_stage, fan_out=True, child_id=child_id)

    # The child state's artifact_dir is NOT the parent's artifact_dir
    assert cs.artifact_dir != parent_artifact_dir
    # At this point the child's artifact dir exists but is empty
    assert cs.artifact_dir.exists()
    assert list(cs.artifact_dir.iterdir()) == []

    # 2. Pass the child state to fork() — this is the bug path
    async def _fork():
        return await gremlin.fork(
            cs,
            child_id,
            parent_id=gremlin_id,
            group_name="mygroup",
            child_key="child-x",
        )

    forked = asyncio.run(_fork())

    try:
        # The forked child should have the parent's artifacts, not an empty dir
        assert (forked.artifact_dir / "plan.md").exists()
        assert (
            forked.artifact_dir / "plan.md"
        ).read_text() == "# Plan\nSome plan content"
        assert (forked.artifact_dir / "spec.md").exists()
        assert (forked.artifact_dir / "spec.md").read_text() == "# Spec\nSome spec"

        # And the child's registry should be a copy of the parent's registry
        child_registry_path = pathlib.Path(scratch_root(child_id)) / "registry.json"
        assert child_registry_path.exists()
        child_reg_data = json.loads(child_registry_path.read_text(encoding="utf-8"))
        assert "artifact://plan.md" in child_reg_data
        assert "artifact://spec.md" in child_reg_data
    finally:
        if forked.worktree and forked.worktree.exists():
            subprocess.run(
                ["git", "worktree", "remove", "--force", str(forked.worktree)],
                cwd=tmp_repo,
                capture_output=True,
            )
