"""Tests for Gremlin.fork() method."""

import asyncio
import json
import subprocess

import pytest
from _gremlins_core.artifacts import ArtifactRegistry
from _gremlins_core.schemas import Pipeline

from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import StateData, build_state
from _gremlins_core.stages import Exec
from tests.fake_client import FakeClient


@pytest.fixture
def tmp_repo(tmp_path):
    """Create a minimal git repo for testing."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    subprocess.run(["git", "init"], cwd=repo_dir, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "test@example.com"],
        cwd=repo_dir,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test User"],
        cwd=repo_dir,
        check=True,
        capture_output=True,
    )
    (repo_dir / "file.txt").write_text("initial")
    subprocess.run(
        ["git", "add", "file.txt"], cwd=repo_dir, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "initial"],
        cwd=repo_dir,
        check=True,
        capture_output=True,
    )
    return repo_dir


def _sandbox(tmp_path, monkeypatch):
    """Point state_root and scratch_root at tmp_path via GREMLINS_SANDBOX_ROOT."""
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))


def test_fork_without_worktree(tmp_path, tmp_repo, monkeypatch):
    """Test forking a state without a worktree."""
    _sandbox(tmp_path, monkeypatch)

    async def _test():
        # Setup source gremlin and state
        state_dir = tmp_path / "state" / "gr-1"
        artifact_dir = tmp_path / "scratch" / "gr-1" / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        # Create some artifacts
        (artifact_dir / "spec.md").write_text("# Spec\n")
        registry = ArtifactRegistry(artifact_dir=artifact_dir)
        registry._set("spec", str(artifact_dir / "spec.md"))

        # Create state
        state_data = StateData(gremlin_id="gr-1")
        state = build_state(
            data=state_data,
            client=FakeClient(),
            artifact_dir=artifact_dir,
            cwd=str(tmp_repo),
            worktree=None,
            artifacts=registry,
        )

        # Create minimal gremlin
        gremlin = Gremlin(
            stages=[],
            state_dir=state_dir,
            gremlin_id="gr-1",
            pipeline_data=Pipeline(name="test", path=tmp_path, stages=[]),
            project_root=str(tmp_repo),
        )
        gremlin.registry = registry

        # Fork the state
        forked = await gremlin.fork(state, "gr-2")

        # Verify the fork
        assert forked.data.gremlin_id == "gr-2"
        assert forked.artifact_dir == tmp_path / "scratch" / "gr-2" / "artifacts"
        assert (forked.artifact_dir / "spec.md").read_text() == "# Spec\n"
        assert forked.worktree is None
        assert forked.base_ref == state.base_ref

        # Verify source is not mutated
        assert state.data.gremlin_id == "gr-1"
        assert state.artifact_dir == artifact_dir

    asyncio.run(_test())


def test_fork_with_worktree(tmp_path, tmp_repo, monkeypatch):
    """Test forking a state with a worktree."""
    _sandbox(tmp_path, monkeypatch)

    async def _test():
        # Create a worktree for the source
        worktree_parent = tmp_path / "worktrees"
        worktree_parent.mkdir()
        worktree_path = worktree_parent / "aibg-gremlin.test1"
        subprocess.run(
            ["git", "worktree", "add", "--detach", str(worktree_path), "HEAD"],
            cwd=tmp_repo,
            check=True,
            capture_output=True,
        )

        # Setup source gremlin and state
        state_dir = tmp_path / "state" / "gr-1"
        artifact_dir = tmp_path / "scratch" / "gr-1" / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        # Create artifacts
        (artifact_dir / "spec.md").write_text("# Spec\n")
        registry = ArtifactRegistry(artifact_dir=artifact_dir)
        registry._set("spec", str(artifact_dir / "spec.md"))

        # Create state with worktree
        state_data = StateData(gremlin_id="gr-1")
        state = build_state(
            data=state_data,
            client=FakeClient(),
            artifact_dir=artifact_dir,
            cwd=str(worktree_path),
            worktree=worktree_path,
            worktree_parent=worktree_parent,
            artifacts=registry,
        )

        # Create minimal gremlin
        gremlin = Gremlin(
            stages=[],
            state_dir=state_dir,
            gremlin_id="gr-1",
            pipeline_data=Pipeline(name="test", path=tmp_path, stages=[]),
            project_root=str(tmp_repo),
        )
        gremlin.registry = registry

        # Fork the state
        forked = await gremlin.fork(state, "gr-2")

        try:
            # Verify the fork
            assert forked.data.gremlin_id == "gr-2"
            assert forked.artifact_dir == tmp_path / "scratch" / "gr-2" / "artifacts"
            assert (forked.artifact_dir / "spec.md").read_text() == "# Spec\n"
            assert forked.worktree is not None
            assert forked.worktree != worktree_path
            assert forked.worktree.exists()
            assert forked.cwd == str(forked.worktree)
            assert forked.base_ref == state.base_ref

            # Verify worktree is at the same commit
            source_sha = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=worktree_path,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            forked_sha = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=forked.worktree,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            assert source_sha == forked_sha

            # Verify source is not mutated
            assert state.data.gremlin_id == "gr-1"
            assert state.artifact_dir == artifact_dir
            assert state.worktree == worktree_path
        finally:
            # Cleanup worktrees
            subprocess.run(
                ["git", "worktree", "remove", "--force", str(worktree_path)],
                cwd=tmp_repo,
                capture_output=True,
            )
            if forked.worktree and forked.worktree.exists():
                subprocess.run(
                    ["git", "worktree", "remove", "--force", str(forked.worktree)],
                    cwd=tmp_repo,
                    capture_output=True,
                )

    asyncio.run(_test())


def test_fork_preserves_registry(tmp_path, tmp_repo, monkeypatch):
    """Test that fork preserves registry.json content."""
    _sandbox(tmp_path, monkeypatch)

    async def _test():
        # Setup source gremlin and state
        state_dir = tmp_path / "state" / "gr-1"
        artifact_dir = tmp_path / "scratch" / "gr-1" / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        # Create registry with multiple bindings
        registry = ArtifactRegistry(artifact_dir=artifact_dir)
        registry._set("spec", str(artifact_dir / "spec.md"))
        registry._set("plan", str(artifact_dir / "plan.md"))
        registry._set("some_key", json.dumps({"data": "value"}))

        # Create state
        state_data = StateData(gremlin_id="gr-1")
        state = build_state(
            data=state_data,
            client=FakeClient(),
            artifact_dir=artifact_dir,
            cwd=str(tmp_repo),
            worktree=None,
            artifacts=registry,
        )

        # Create minimal gremlin
        gremlin = Gremlin(
            stages=[],
            state_dir=state_dir,
            gremlin_id="gr-1",
            pipeline_data=Pipeline(name="test", path=tmp_path, stages=[]),
            project_root=str(tmp_repo),
        )
        gremlin.registry = registry

        # Fork the state
        forked = await gremlin.fork(state, "gr-2")

        # Verify registry is preserved
        assert forked.artifacts.is_registered("spec")
        assert forked.artifacts.is_registered("plan")
        assert forked.artifacts.is_registered("some_key")
        assert forked.artifacts.data_uri("some_key") == json.dumps({"data": "value"})

    asyncio.run(_test())


def test_fork_with_branch_pipeline_scopes_child(tmp_path, tmp_repo, monkeypatch):
    """fork(pipeline=...) writes a branch-scoped pipeline.yaml into the child
    state dir and sets pipeline_path/pipeline_data to it, not the parent's."""
    _sandbox(tmp_path, monkeypatch)

    async def _test():
        parent_pipeline_path = tmp_path / "parent.yaml"
        parent_pipeline_path.write_text(
            "default_client: openai:gpt-4o\nstages:\n  - name: implement\n    type: exec\n"
        )
        parent_pipeline = Pipeline.from_yaml(parent_pipeline_path)

        branch_stage = Exec.with_dict({"name": "poll", "type": "exec", "run": "true"})
        branch_stage.raw_dict = {"name": "poll", "type": "exec", "run": "true"}
        branch_pipeline = Pipeline(
            name="poll",
            path=parent_pipeline_path,
            stages=[branch_stage],
            default_client=None,
            base_ref="current",
        )

        state_dir = tmp_path / "state" / "gr-parent"
        artifact_dir = tmp_path / "scratch" / "gr-parent" / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
        state_dir.mkdir(parents=True, exist_ok=True)

        state_data = StateData(gremlin_id="gr-parent")
        state_data.persist(state_dir, {"pipeline_path": str(parent_pipeline_path)})
        state = build_state(
            data=state_data,
            client=FakeClient(),
            artifact_dir=artifact_dir,
            cwd=str(tmp_repo),
            pipeline_data=parent_pipeline,
        )

        gremlin = Gremlin(
            stages=[],
            state_dir=state_dir,
            gremlin_id="gr-parent",
            pipeline_data=parent_pipeline,
            project_root=str(tmp_repo),
        )

        forked = await gremlin.fork(state, "gr-child", pipeline=branch_pipeline)

        # Child's in-memory pipeline_data is the branch pipeline, not the parent's
        assert forked.pipeline_data is not None
        assert forked.pipeline_data.name == "poll"
        assert len(forked.pipeline_data.stages) == 1
        assert forked.pipeline_data.stages[0].name == "poll"

        # Child's pipeline_path points into its own state dir, not the parent pipeline
        child_state_dir = state_dir.parent / "gr-child"
        assert forked.data.pipeline_path == str(child_state_dir / "pipeline.yaml")
        assert (child_state_dir / "pipeline.yaml").exists()

        # The written YAML contains only the branch stage
        import yaml

        written = yaml.safe_load((child_state_dir / "pipeline.yaml").read_text())
        assert written == {"stages": [{"name": "poll", "type": "exec", "run": "true"}]}

        # Parent state is not mutated
        assert state.data.pipeline_path == str(parent_pipeline_path)

    asyncio.run(_test())
