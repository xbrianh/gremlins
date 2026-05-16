"""Shared helpers for composite stages (Loop, Sequence, Parallel)."""

from __future__ import annotations

import dataclasses

from gremlins.executor.state import State
from gremlins.stages.base import Stage


def child_state(parent: State, child: Stage, *, fan_out: bool = False) -> State:
    """Derive a child State from parent."""
    client = parent.test_client or child.client
    if not fan_out:
        return dataclasses.replace(parent, client=client)
    # fan_out=True: caller pre-sets parent.session_dir to the group dir
    return dataclasses.replace(
        parent,
        client=client,
        session_dir=parent.session_dir / child.name,
        child_key=child.name,
    )
