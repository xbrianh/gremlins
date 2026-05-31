"""Tests for Gremlin.open() constructor."""

from __future__ import annotations

import json
import pathlib
import subprocess

import pytest

from gremlins.executor.gremlin import Gremlin


def _init_git_repo(path: pathlib.Path) -> None:
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


def test_gremlin_open_valid_state(sandbox, project_dir, pipeline_yaml):
    """Gremlin.open() reconstructs a gremlin from persisted state."""
    gremlin_id = "test-open-valid"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)

    # Write state.json
    state_data = {
        "id": gremlin_id,
        "kind": "local",
        "project_root": str(project_dir),
        "pipeline_path": str(pipeline_yaml),
        "pipeline_args": ["--foo", "bar"],
        "instructions": "test instructions",
        "spec": "spec-value",
        "plan": "plan-value",
        "repo": "test-repo",
        "base_ref_sha": "abc123",
        "base_ref": "main",
        "workdir": "/tmp/worktree",
        "resume_from": "test",
    }
    (state_dir / "state.json").write_text(json.dumps(state_data), encoding="utf-8")

    gremlin = Gremlin.open(gremlin_id)

    assert gremlin.gremlin_id == gremlin_id
    assert gremlin.project_root == str(project_dir)
    assert gremlin.instructions == "test instructions"
    assert gremlin.spec == "spec-value"
    assert gremlin.plan == "plan-value"
    assert gremlin.repo == "test-repo"
    assert gremlin.base_ref_sha == "abc123"
    assert gremlin.base_ref == "main"
    assert gremlin.resume_from == "test"
    assert gremlin.worktree_dir == pathlib.Path("/tmp/worktree")


def test_gremlin_open_nonexistent_state_raises(sandbox):
    """Gremlin.open() raises FileNotFoundError for nonexistent state directory."""
    with pytest.raises(FileNotFoundError, match="no state at"):
        Gremlin.open("nonexistent-id")


def test_gremlin_open_missing_state_json_raises(sandbox):
    """Gremlin.open() raises FileNotFoundError if state.json is missing."""
    gremlin_id = "test-open-no-json"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)

    with pytest.raises(FileNotFoundError, match="no state.json"):
        Gremlin.open(gremlin_id)


def test_gremlin_open_malformed_json_raises(sandbox):
    """Gremlin.open() raises ValueError for malformed state.json."""
    gremlin_id = "test-open-bad-json"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)

    (state_dir / "state.json").write_text("not valid json", encoding="utf-8")

    with pytest.raises(ValueError, match="could not parse state.json"):
        Gremlin.open(gremlin_id)


def test_gremlin_open_with_hermetic_pipeline(sandbox, project_dir):
    """Gremlin.open() uses hermetic pipeline.yaml if present."""
    gremlin_id = "test-open-hermetic"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)

    # Create a hermetic pipeline
    pipeline_yaml = state_dir / "pipeline.yaml"
    pipeline_yaml.write_text(
        """\
stages:
  - name: test
    type: exec
    options:
      cmds:
        - "true"
"""
    )

    state_data = {
        "id": gremlin_id,
        "kind": "local",
        "project_root": str(project_dir),
        "pipeline_path": "/some/other/path.yaml",
        "pipeline_args": [],
        "instructions": "",
    }
    (state_dir / "state.json").write_text(json.dumps(state_data), encoding="utf-8")

    gremlin = Gremlin.open(gremlin_id)

    assert gremlin.gremlin_id == gremlin_id
    # Verify the pipeline was loaded from the hermetic path
    assert gremlin.pipeline_data is not None
    assert gremlin.pipeline_data.path == pipeline_yaml.resolve()


def test_gremlin_open_sets_all_fields(sandbox, project_dir, pipeline_yaml):
    """Gremlin.open() properly sets all constructor fields."""
    gremlin_id = "test-open-all-fields"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)

    state_data = {
        "id": gremlin_id,
        "kind": "local",
        "project_root": str(project_dir),
        "pipeline_path": str(pipeline_yaml),
        "pipeline_args": ["--arg1", "val1"],
        "instructions": "test instr",
        "spec": "spec-file",
        "plan": "plan-file",
        "repo": "test-repo",
        "base_ref_sha": "def456",
        "base_ref": "develop",
        "workdir": "/tmp/work",
        "worktree_parent": "/tmp/parent",
        "resume_from": "stage-name",
    }
    (state_dir / "state.json").write_text(json.dumps(state_data), encoding="utf-8")

    gremlin = Gremlin.open(gremlin_id)

    assert gremlin.gremlin_id == gremlin_id
    assert gremlin.state_dir == state_dir
    assert gremlin.project_root == str(project_dir)
    assert gremlin.instructions == "test instr"
    assert gremlin.spec == "spec-file"
    assert gremlin.plan == "plan-file"
    assert gremlin.repo == "test-repo"
    assert gremlin.base_ref_sha == "def456"
    assert gremlin.base_ref == "develop"
    assert gremlin.worktree_dir == pathlib.Path("/tmp/work")
    assert gremlin.worktree_parent == pathlib.Path("/tmp/parent")
    assert gremlin.resume_from == "stage-name"
