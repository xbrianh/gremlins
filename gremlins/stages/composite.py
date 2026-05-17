"""Shared helpers for composite stages (Loop, Sequence, Parallel)."""

from __future__ import annotations

import dataclasses

from gremlins.executor.state import State
from gremlins.stages.base import Stage


def child_state(parent: State, child: Stage, *, fan_out: bool = False) -> State:
    """Derive a child State from parent."""
    client = parent.test_client or child.client
    stage_model = (
        child.client.model
        if child.client and parent.test_client
        else parent.stage_model
    )
    if not fan_out:
        return dataclasses.replace(parent, client=client, stage_model=stage_model)
    # fan_out=True: caller pre-sets parent.session_dir to the group dir
    return dataclasses.replace(
        parent,
        client=client,
        stage_model=stage_model,
        session_dir=parent.session_dir / child.name,
        child_key=child.name,
    )
