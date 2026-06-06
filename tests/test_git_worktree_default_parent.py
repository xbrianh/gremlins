"""Tests that worktree-setup functions default to paths.work_root()."""

from __future__ import annotations

import os
import pathlib
import subprocess

from gremlins import paths
from gremlins.utils.git import setup_detached_worktree

_GIT_ENV = {
    **os.environ,
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@t",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@t",
}


def _init_repo(path: pathlib.Path) -> None:
    subprocess.run(
        ["git", "init", str(path)], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(path), "commit", "--allow-empty", "-m", "init"],
        check=True,
        capture_output=True,
        env=_GIT_ENV,
    )


def test_setup_detached_worktree_defaults_to_work_root(tmp_path: pathlib.Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    workdir = setup_detached_worktree(str(repo), "HEAD")
    try:
        assert pathlib.Path(workdir).parent == paths.work_root()
    finally:
        subprocess.run(
            ["git", "worktree", "remove", "--force", workdir],
            cwd=str(repo),
            capture_output=True,
        )
