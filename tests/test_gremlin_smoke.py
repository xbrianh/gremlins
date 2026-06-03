"""Smoke test: programmatic Gremlin API with no launcher involvement."""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import shutil
import subprocess

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import StateData
from gremlins.pipeline import Pipeline
from gremlins.stages.exec import Exec

TRIVIAL_PIPELINE = """\
stages:
  - name: smoke
    type: exec
    options:
      cmds:
        - "true"
"""


@pytest.fixture()
def project_dir(tmp_path):
    """Git repository for testing."""
    d = tmp_path / "project"
    d.mkdir()
    (d / "file.txt").write_text("hello")
    subprocess.run(["git", "init"], cwd=str(d), check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=str(d),
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=str(d),
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "add", "file.txt"],
        cwd=str(d),
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "commit", "-m", "init"],
        cwd=str(d),
        check=True,
        capture_output=True,
    )
    return d


@pytest.fixture()
def pipeline_yaml(tmp_path):
    p = tmp_path / "trivial.yaml"
    p.write_text(TRIVIAL_PIPELINE)
    return p


def test_gremlin_run_in_process(project_dir, pipeline_yaml, sandbox):
    gremlin_id = "smoke-abc123"
    sd = sandbox.state / gremlin_id

    saved_cwd = os.getcwd()
    worktree = None
    rc = 1
    try:
        gremlin = Gremlin.initialize_with_runtime(
            gremlin_id=gremlin_id,
            state_dir=sd,
            project_dir=project_dir,
            pipeline_ref=str(pipeline_yaml),
            project_root=str(project_dir),
        )
        worktree = gremlin.worktree
        asyncio.run(gremlin.run())
        rc = 0
    finally:
        os.chdir(saved_cwd)
        StateData.load(gremlin_id).write_terminal_state(rc)
        if worktree and worktree.is_dir():
            shutil.rmtree(worktree, ignore_errors=True)

    assert sd.is_dir()
    data = json.loads((sd / "state.json").read_text())
    assert data.get("status") == "done"
    assert data.get("stage") == "smoke"


def test_resume_unbinds_stale_exec_out_keys(tmp_path):
    artifact_dir = tmp_path / "artifacts"
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    stage = Exec("normalize", {}, out_map={"normalize-commits": "git://range"})
    pipeline = Pipeline(name="test", path=tmp_path, stages=[stage])
    gremlin = Gremlin(
        [stage],
        state_dir=state_dir,
        gremlin_id=None,
        pipeline_data=pipeline,
        resume_from="normalize",
    )
    gremlin.registry = ArtifactRegistry(artifact_dir=artifact_dir)
    gremlin.registry.bind("normalize-commits", Uri.parse("git://range/old..stale"))

    assert gremlin.registry.produced("normalize-commits")
    gremlin._unbind_stale_exec_artifacts()
    assert not gremlin.registry.produced("normalize-commits")


def test_resume_unbind_only_affects_exec_stages(tmp_path):
    artifact_dir = tmp_path / "artifacts"
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    exec_stage = Exec("work", {}, out_map={"work-out": "git://range"})
    pipeline = Pipeline(name="test", path=tmp_path, stages=[exec_stage])
    gremlin = Gremlin(
        [exec_stage],
        state_dir=state_dir,
        gremlin_id=None,
        pipeline_data=pipeline,
        resume_from="work",
    )
    gremlin.registry = ArtifactRegistry(artifact_dir=artifact_dir)
    gremlin.registry.bind("work-out", Uri.parse("git://range/a..b"))
    gremlin.registry.bind("non-exec-artifact", Uri.parse("git://range/x..y"))

    gremlin._unbind_stale_exec_artifacts()
    assert not gremlin.registry.produced("work-out")
    assert gremlin.registry.produced("non-exec-artifact")


def test_gremlin_state_delegates_after_initialize(project_dir, pipeline_yaml, sandbox):
    gremlin_id = "state-delegates-abc123"
    sd = sandbox.state / gremlin_id

    saved_cwd = os.getcwd()
    worktree = None
    try:
        gremlin = Gremlin.initialize_with_runtime(
            gremlin_id=gremlin_id,
            state_dir=sd,
            project_dir=project_dir,
            pipeline_ref=str(pipeline_yaml),
            project_root=str(project_dir),
            base_ref="main",
            repo="test/repo",
        )
        worktree = gremlin.worktree

        assert gremlin.state is not None
        assert isinstance(gremlin.artifact_dir, pathlib.Path)
        assert gremlin.artifact_dir == sd / "artifacts"
        assert gremlin.artifacts is not None
        assert isinstance(gremlin.artifacts, ArtifactRegistry)
        assert gremlin.client is not None
        assert isinstance(gremlin.cwd, str)
        assert gremlin.base_ref == "main"
        assert gremlin.repo == "test/repo"
        assert gremlin.loop_iteration == 1
        assert gremlin.attempt == ""
    finally:
        os.chdir(saved_cwd)
        if worktree and worktree.is_dir():
            shutil.rmtree(worktree, ignore_errors=True)


def test_gremlin_state_unchanged_after_run(project_dir, pipeline_yaml, sandbox):
    gremlin_id = "state-run-abc123"
    sd = sandbox.state / gremlin_id

    saved_cwd = os.getcwd()
    worktree = None
    rc = 1
    try:
        gremlin = Gremlin.initialize_with_runtime(
            gremlin_id=gremlin_id,
            state_dir=sd,
            project_dir=project_dir,
            pipeline_ref=str(pipeline_yaml),
            project_root=str(project_dir),
            base_ref="main",
            repo="test/repo",
        )
        worktree = gremlin.worktree

        initial_client = gremlin.client
        initial_base_ref = gremlin.base_ref
        initial_repo = gremlin.repo

        asyncio.run(gremlin.run())
        rc = 0

        assert gremlin.client == initial_client
        assert gremlin.base_ref == initial_base_ref
        assert gremlin.repo == initial_repo
    finally:
        os.chdir(saved_cwd)
        StateData.load(gremlin_id).write_terminal_state(rc)
        if worktree and worktree.is_dir():
            shutil.rmtree(worktree, ignore_errors=True)
