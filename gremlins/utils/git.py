"""Filesystem orchestration around git worktrees.

The git operations themselves — predicates, readers, and mutations — live in
the native extension module ``_gremlins_core.utils.git`` (backed by
``crates/gremlins/src/core/git.rs``). Import them from there directly; this
module owns only the two helpers that mix git with filesystem layout:

- :func:`setup_workdir` — assert the project is a repo, add a detached worktree,
  then stage the project's ``.gremlins`` overlay into the state directory.
- :func:`stage_gremlins_overlay` — copy the project overlay into the state dir.

``GitError`` is re-exported so callers that already import it from here keep
working.
"""

from __future__ import annotations

import os
import pathlib
import shutil

from _gremlins_core.config import overlay_dirname
from _gremlins_core.utils.git import GitError, in_git_repo, setup_detached_worktree

__all__ = ["stage_gremlins_overlay", "setup_workdir", "GitError"]


def stage_gremlins_overlay(project_root: str, state_dir: os.PathLike[str]) -> None:
    """Copy the project's ``.gremlins`` overlay into ``state_dir``.

    Nothing is copied when the project has no overlay, or when the overlay
    already *is* the destination (a state dir inside the project).
    """
    dirname = overlay_dirname()
    src = pathlib.Path(project_root) / dirname
    dst = pathlib.Path(state_dir) / dirname
    if src.is_dir() and src.resolve() != dst.resolve():
        shutil.copytree(src, dst, dirs_exist_ok=True)


def setup_workdir(
    project_root: str,
    base_ref: str,
    *,
    fetch: bool = False,
    state_dir: os.PathLike[str],
    worktree_parent: pathlib.Path | None = None,
) -> str:
    """Set up a detached worktree and stage the overlay into ``state_dir``.

    ``base_ref`` may be empty, meaning "the current HEAD". With ``fetch=True``
    the ref is fetched from ``origin`` first. Returns the worktree path.
    """
    if not in_git_repo(cwd=project_root):
        raise GitError(128, f"{project_root!r} is not a git repository")

    workdir = setup_detached_worktree(
        project_root,
        base_ref if base_ref else "HEAD",
        fetch=fetch,
        worktree_parent=worktree_parent,
    )
    stage_gremlins_overlay(project_root, state_dir)
    return workdir
