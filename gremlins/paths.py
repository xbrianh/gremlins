"""Single source of truth for gremlins state-directory resolution; everything that needs the per-user state location goes through ``state_root()``."""

from __future__ import annotations

import pathlib

import platformdirs


def state_root() -> pathlib.Path:
    """Return the per-user gremlins state directory."""
    path = pathlib.Path(platformdirs.user_state_dir("gremlins"))
    path.mkdir(parents=True, exist_ok=True)
    return path
