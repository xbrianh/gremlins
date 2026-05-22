"""Single source of truth for gremlins state-directory resolution; everything that needs the per-user state location goes through ``state_root()``."""

from __future__ import annotations

import pathlib
import tempfile


def state_root() -> pathlib.Path:
    """Return the per-user gremlins state directory."""
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
    path = pathlib.Path(tempfile.gettempdir()) / "gremlins"
    path.mkdir(parents=True, exist_ok=True)
    return path
