"""Shared helpers for composite stages (Loop, Sequence, Parallel)."""

from __future__ import annotations

import dataclasses

from gremlins.executor.state import State
from gremlins.stages.base import Stage


def child_state(
    parent: State, child: Stage, *, fan_out: bool = False, child_id: str | None = None
) -> State:
    """Derive a child State from parent."""
    client = parent.test_client or child.client or parent.client
    stage_model = (
        child.client.model
        if child.client and parent.test_client
        else parent.stage_model
    )
    # Old layout (no gremlin_id): child artifact_dir = parent.artifact_dir / child.name
    if fan_out and not child_id:
        artifact_dir = parent.artifact_dir / child.name
        artifact_dir.mkdir(parents=True, exist_ok=True)
        return dataclasses.replace(
            parent,
            client=client,
            stage_model=stage_model,
            artifact_dir=artifact_dir,
            child_key=child.name,
        )
    return dataclasses.replace(
        parent, client=client, stage_model=stage_model, child_key=child.name
    )
