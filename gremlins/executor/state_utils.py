from __future__ import annotations

import datetime
import pathlib
import re
import secrets
from typing import Any

from gremlins import paths as _paths

_GREMLIN_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def validate_gremlin_id(gremlin_id: str) -> None:
    """Raise ValueError if gremlin_id is not a safe, non-path-traversing identifier."""
    if ".." in gremlin_id or not _GREMLIN_ID_RE.match(gremlin_id):
        raise ValueError(f"gremlin_id contains illegal characters: {gremlin_id!r}")


def resolve_state_file(gremlin_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gremlin_id, or None when gremlin_id is absent."""
    if not gremlin_id:
        return None
    return _paths.state_root() / gremlin_id / "state.json"


def resolve_artifact_dir(gremlin_id: str | None = None) -> pathlib.Path:
    """Resolve the artifacts directory for the current run."""
    state_root = _paths.state_root()
    if gremlin_id:
        artifact_dir = state_root / gremlin_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        artifact_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    return artifact_dir


def landable_shape(state: dict[str, Any]) -> str:
    """Classify artifact shape for land dispatch."""
    artifacts = list(state.get("artifacts") or [])
    prs = [art for art in artifacts if art.get("type") == "pr"]

    if not prs:
        return "empty"
    if len(prs) == 1:
        return "one_pr"
    return "many_prs"
