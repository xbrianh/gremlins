"""Shared helpers for composite stages (Loop, Sequence, Parallel)."""

from __future__ import annotations

import dataclasses

from gremlins import paths as _paths
from gremlins.clients.client import PACKAGE_DEFAULT
from gremlins.executor.state import State
from gremlins.stages.base import Stage


def child_state(
    parent_state: State,
    child: Stage,
    *,
    fan_out: bool = False,
    child_id: str | None = None,
) -> State:
    """Derive a child State from parent."""
    client = parent_state.client
    if child.client is not None and child.client != PACKAGE_DEFAULT:
        client = child.client
    if not fan_out:
        return dataclasses.replace(parent_state, client=client)
    if child_id:
        artifact_dir = _paths.state_root() / child_id / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
    else:
        artifact_dir = parent_state.artifact_dir / child.name
        artifact_dir.mkdir(parents=True, exist_ok=True)
    return dataclasses.replace(
        parent_state,
        client=client,
        artifact_dir=artifact_dir,
        child_key=child.name,
    )
