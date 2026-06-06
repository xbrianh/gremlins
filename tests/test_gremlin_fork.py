"""Tests for Gremlin.fork() method."""

from conftest import _TestGremlin
import asyncio
import subprocess

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.stages.exec import Exec


@pytest.fixture
def tmp_repo(tmp_path):
    """Create a minimal git repo for testing."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    subprocess.run(_TestGremlin(["git", "init"], cwd=repo_dir, check=True, capture_output=True))
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


def test_fork_without_worktree(tmp_path, tmp_repo):
    """Test forking a state without a worktree."""

    async def _test():
        # Setup source gremlin and state
        state_dir = tmp_path / "state" / "gr-1"
        artifact_dir = state_dir / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        # Create some artifacts
        (artifact_dir / "spec.md").write_text("# Spec\n")
        registry = ArtifactRegistry(artifact_dir=artifact_dir, cwd=None)
        registry.bind("spec", Uri.parse("file://session/spec.md"))

        # Create state
        state_data = StateData(gremlin_id="gr-1")
        state = build_state(
            data=state_data,
            client=FakeClaudeClient(),
            artifact_dir=artifact_dir,
            repo="test-repo",
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
        assert forked.artifact_dir == state_dir.parent / "gr-2" / "artifacts"
        assert (forked.artifact_dir / "spec.md").read_text() == "# Spec\n"
        assert forked.worktree is None
        assert forked.repo == state.repo
        assert forked.base_ref == state.base_ref

        # Verify source is not mutated
        assert state.data.gremlin_id == "gr-1"
        assert state.artifact_dir == artifact_dir

    asyncio.run(_TestGremlin(_test()))


def test_fork_with_worktree(tmp_path, tmp_repo):
    """Test forking a state with a worktree."""

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
        artifact_dir = state_dir / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        # Create artifacts
        (artifact_dir / "spec.md").write_text("# Spec\n")
        registry = ArtifactRegistry(artifact_dir=artifact_dir, cwd=worktree_path)
        registry.bind("spec", Uri.parse("file://session/spec.md"))

        # Create state with worktree
        state_data = StateData(gremlin_id="gr-1")
        state = build_state(
            data=state_data,
            client=FakeClaudeClient(),
            artifact_dir=artifact_dir,
            repo="test-repo",
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
            assert forked.artifact_dir == state_dir.parent / "gr-2" / "artifacts"
            assert (forked.artifact_dir / "spec.md").read_text() == "# Spec\n"
            assert forked.worktree is not None
            assert forked.worktree != worktree_path
            assert forked.worktree.exists()
            assert forked.cwd == str(forked.worktree)
            assert forked.repo == state.repo
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

    asyncio.run(_TestGremlin(_test()))


def test_fork_preserves_registry(tmp_path, tmp_repo):
    """Test that fork preserves registry.json content."""

    async def _test():
        # Setup source gremlin and state
        state_dir = tmp_path / "state" / "gr-1"
        artifact_dir = state_dir / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        # Create registry with multiple bindings
        registry = ArtifactRegistry(artifact_dir=artifact_dir, cwd=None)
        registry.bind("spec", Uri.parse("file://session/spec.md"))
        registry.bind("plan", Uri.parse("file://session/plan.md"))
        registry.write("some_key", {"data": "value"})

        # Create state
        state_data = StateData(gremlin_id="gr-1")
        state = build_state(
            data=state_data,
            client=FakeClaudeClient(),
            artifact_dir=artifact_dir,
            repo="test-repo",
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
        assert "spec" in forked.artifacts.data
        assert "plan" in forked.artifacts.data
        assert "some_key" in forked.artifacts.data
        assert forked.artifacts.read("some_key") == {"data": "value"}

    asyncio.run(_TestGremlin(_test()))


def test_fork_with_branch_pipeline_scopes_child(tmp_path, tmp_repo):
    """fork(pipeline=...) writes a branch-scoped pipeline.yaml into the child
    state dir and sets pipeline_path/pipeline_data to it, not the parent's."""

    async def _test():
        parent_pipeline_path = tmp_path / "parent.yaml"
        parent_pipeline_path.write_text(
            "stages:\n  - name: implement\n    type: exec\n"
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
        artifact_dir = state_dir / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)

        state_data = StateData(
            gremlin_id="gr-parent", pipeline_path=str(parent_pipeline_path)
        )
        state = build_state(
            data=state_data,
            client=FakeClaudeClient(),
            artifact_dir=artifact_dir,
            repo="test-repo",
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

    asyncio.run(_TestGremlin(_test()))
