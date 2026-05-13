"""Smoke test: programmatic Gremlin API with no launcher involvement."""

from __future__ import annotations

import json
import os
import shutil

import pytest

from gremlins.executor.gremlin import Gremlin
from gremlins.executor.state import State

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


def test_gremlin_run_in_process(project_dir, pipeline_yaml, test_state_root, tmp_path):
    gr_id = "smoke-abc123"
    sd = test_state_root / gr_id

    gremlin = Gremlin.build(
        gr_id=gr_id,
        state_dir=sd,
        project_dir=project_dir,
        pipeline_ref=str(pipeline_yaml),
        project_root=str(project_dir),
    )

    saved_cwd = os.getcwd()
    try:
        gremlin.initialize_runtime()
        gremlin.run()
    finally:
        os.chdir(saved_cwd)

    worktree = gremlin.worktree_dir
    try:
        assert sd.is_dir()
        assert worktree is not None and worktree.is_dir()

        State.load(gr_id).write_terminal_state(0)

        data = json.loads((sd / "state.json").read_text())
        assert data.get("status") == "done"
        assert data.get("stage") == "smoke"
    finally:
        # setup_copy creates a temp dir outside tmp_path; clean it up
        if worktree:
            shutil.rmtree(worktree, ignore_errors=True)
