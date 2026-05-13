"""Smoke test: programmatic Gremlin API with no launcher involvement."""

from __future__ import annotations

import json
import os
import shutil

import pytest

from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import StateData

TRIVIAL_PIPELINE = """\
stages:
  - name: smoke
    type: run-cmd
    options:
      cmds:
        - "true"
"""


@pytest.fixture()
def project_dir(tmp_path):
    """Non-git directory so initialize_runtime() uses setup_copy."""
    d = tmp_path / "project"
    d.mkdir()
    (d / "file.txt").write_text("hello")
    return d


@pytest.fixture()
def pipeline_yaml(tmp_path):
    p = tmp_path / "trivial.yaml"
    p.write_text(TRIVIAL_PIPELINE)
    return p


def test_gremlin_run_in_process(project_dir, pipeline_yaml, test_state_root):
    gremlin_id = "smoke-abc123"
    sd = test_state_root / gremlin_id

    gremlin = Gremlin.build(
        gremlin_id=gremlin_id,
        state_dir=sd,
        project_dir=project_dir,
        pipeline_ref=str(pipeline_yaml),
        project_root=str(project_dir),
    )

    saved_cwd = os.getcwd()
    worktree = None
    rc = 1
    try:
        gremlin.initialize_runtime()
        worktree = gremlin.worktree_dir
        gremlin.run()
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
