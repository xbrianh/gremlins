"""Single source of truth for gremlins filesystem locations.

Override mechanism
------------------
Every resolver honours one or more environment variables so that a test
fixture can redirect all I/O to a sandbox directory without touching
production code paths.

  GREMLINS_SANDBOX_ROOT   — re-bases state_root(), work_root(), and
                            user_config_root() under a single directory.
                            When set, each resolver returns a sub-path
                            inside GREMLINS_SANDBOX_ROOT instead of its
                            normal system location.

  GREMLINS_PROJECT_ROOT   — overrides project_root(), the "where the user
                            invoked gremlins" directory (default: cwd).

  GREMLINS_OVERLAY_DIR    — overrides project_overlay_dir(), the per-project
                            .gremlins config directory.
"""

from __future__ import annotations

import os
import pathlib
import tempfile

OVERLAY_DIRNAME = ".gremlins"


def state_root() -> pathlib.Path:
    """Return the per-user gremlins state directory."""
    sandbox = os.environ.get("GREMLINS_SANDBOX_ROOT", "")
    if sandbox:
        path = pathlib.Path(sandbox) / "state"
    else:
        import platformdirs

        path = pathlib.Path(platformdirs.user_state_dir("gremlins"))
    path.mkdir(parents=True, exist_ok=True)
    return path


def work_root() -> pathlib.Path:
    """Return the directory where gremlin worktrees are created by default.

    Lives under the system temp dir so orphaned worktrees (from crashed or
    killed gremlins) get cleaned up by the OS on reboot, and so a single
    ambient permission rule covers every worktree path.
    """
    sandbox = os.environ.get("GREMLINS_SANDBOX_ROOT", "")
    if sandbox:
        path = pathlib.Path(sandbox) / "work"
    else:
        path = pathlib.Path(tempfile.gettempdir()) / "gremlins"
    path.mkdir(parents=True, exist_ok=True)
    return path


def user_config_root() -> pathlib.Path:
    """Return the per-user gremlins config directory."""
    sandbox = os.environ.get("GREMLINS_SANDBOX_ROOT", "")
    if sandbox:
        return pathlib.Path(sandbox) / "config"
    return pathlib.Path.home() / ".config" / "gremlins"


def project_root() -> pathlib.Path:
    """Return the project root (where the user invoked gremlins)."""
    override = os.environ.get("GREMLINS_PROJECT_ROOT", "")
    if override:
        return pathlib.Path(override)
    return pathlib.Path.cwd()


def project_overlay_dir(project_root: pathlib.Path) -> pathlib.Path:
    """Return the .gremlins overlay directory for a project."""
    override = os.environ.get("GREMLINS_OVERLAY_DIR", "")
    if override:
        return pathlib.Path(override)
    return project_root / OVERLAY_DIRNAME


def expand_user_path(s: str) -> str:
    """Expand a leading ~ in a path string."""
    return str(pathlib.Path(s).expanduser())
