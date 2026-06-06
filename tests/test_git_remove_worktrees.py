"""Tests for remove_worktrees_async."""

from __future__ import annotations

from conftest import _TestGremlin
import asyncio
import os
import pathlib
import subprocess

from gremlins.utils.git import remove_worktrees_async


def test_noop_outside_git_repo(tmp_path: pathlib.Path) -> None:
    asyncio.run(_TestGremlin(remove_worktrees_async(str(tmp_path), [str(tmp_path / "nonexistent")])))


def test_handles_missing_path(tmp_path: pathlib.Path) -> None:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@t",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@t",
    }
    subprocess.run(_TestGremlin(["git", "init", str(tmp_path)], check=True, capture_output=True))
    subprocess.run(
        ["git", "-C", str(tmp_path), "commit", "--allow-empty", "-m", "init"],
        check=True,
        capture_output=True,
        env=env,
    )
    asyncio.run(
        remove_worktrees_async(str(tmp_path), [str(tmp_path / "no-such-worktree")])
    )
