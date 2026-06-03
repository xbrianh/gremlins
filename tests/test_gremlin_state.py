"""Tests for Gremlin.state attribute."""

from __future__ import annotations

import asyncio
import subprocess

import pytest

from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import State
from gremlins.protocols import StageProtocol
from gremlins.stages.outcome import Done


def _init_git_repo(path) -> None:
    subprocess.run(["git", "init"], cwd=str(path), check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=str(path),
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=str(path),
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "add", "-A"],
        cwd=str(path),
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "commit", "-m", "init"],
        cwd=str(path),
        check=True,
        capture_output=True,
    )


@pytest.fixture
def project_dir(tmp_path):
    """Git repository for testing."""
    d = tmp_path / "project"
    d.mkdir()
    (d / "file.txt").write_text("hello")
    _init_git_repo(d)
    return d


@pytest.fixture
def pipeline_yaml(tmp_path):
    p = tmp_path / "trivial.yaml"
    p.write_text(
        """\
stages:
  - name: test
    type: exec
    options:
      cmds:
        - "true"
"""
    )
    return p


def test_gremlin_state_populated_after_initialize(project_dir, pipeline_yaml, sandbox):
    """Gremlin.state is non-None after initialize_with_runtime."""
    gremlin_id = "test-state-init"
    state_dir = sandbox.state / gremlin_id

    gremlin = Gremlin.initialize_with_runtime(
        gremlin_id=gremlin_id,
        state_dir=state_dir,
        project_dir=project_dir,
        pipeline_ref=str(pipeline_yaml),
        project_root=str(project_dir),
    )

    assert gremlin.state is not None


def test_gremlin_state_attributes_accessible(project_dir, pipeline_yaml, sandbox):
    """Gremlin.state attributes are accessible after initialize_with_runtime."""
    gremlin_id = "test-state-attrs"
    state_dir = sandbox.state / gremlin_id

    gremlin = Gremlin.initialize_with_runtime(
        gremlin_id=gremlin_id,
        state_dir=state_dir,
        project_dir=project_dir,
        pipeline_ref=str(pipeline_yaml),
        project_root=str(project_dir),
    )

    assert gremlin.state is not None
    assert gremlin.state.client is not None
    assert gremlin.state.artifacts is not None
    assert gremlin.state.artifact_dir is not None


class StateCapturingStage(StageProtocol):
    """Test stage that captures gremlin.state when run."""

    def __init__(self):
        self.name = "capture"
        self.type = "test"
        self.path = "capture"
        self.gremlin = None
        self.client = None
        self.captured_state = None
        self.skip_if_exists = None

    async def run(self, state: State):
        """Capture the gremlin.state value during execution."""
        if self.gremlin:
            self.captured_state = self.gremlin.state
        return Done()


def test_gremlin_state_set_before_stage_run(project_dir, pipeline_yaml, sandbox):
    """Gremlin.state is set before stage.run() is called."""
    gremlin_id = "test-state-runner"
    state_dir = sandbox.state / gremlin_id

    gremlin = Gremlin.initialize_with_runtime(
        gremlin_id=gremlin_id,
        state_dir=state_dir,
        project_dir=project_dir,
        pipeline_ref=str(pipeline_yaml),
        project_root=str(project_dir),
    )

    stage = StateCapturingStage()

    collected = gremlin._collect_stages([stage])
    assert len(collected) > 0
    assert collected[0][0] == "capture"

    asyncio.run(collected[0][1]())
    assert stage.captured_state is not None
    assert stage.captured_state == gremlin.state
