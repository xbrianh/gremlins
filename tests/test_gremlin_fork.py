"""Tests for Gremlin.fork() method."""

import asyncio
import json
import pathlib
import subprocess
import tempfile

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline


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
        ["git", "commit", "-m", "initial"], cwd=repo_dir, check=True, capture_output=True
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

    asyncio.run(_test())


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

    asyncio.run(_test())


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

    asyncio.run(_test())
