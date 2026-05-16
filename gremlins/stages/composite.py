"""Shared helpers for composite stages (Loop, Sequence, Parallel)."""

from __future__ import annotations

import dataclasses

from gremlins.executor.state import State
from gremlins.stages.base import Stage


def child_state(parent: State, child: Stage, *, fan_out: bool = False) -> State:
    """Derive a child State from parent.

    fan_out=True: parallel children get their own session_dir and child_key
    so per-child artifacts land in <parent_session>/<group>/<child>/.
    Worktree assignment for fan_out=True is the caller's job (Parallel sets
    child_state.worktree after this returns).
    """
    client = parent.test_client or child.client
    if not fan_out:
        return dataclasses.replace(parent, client=client)
    # child.path is "group/child_name" — set by ParallelStage.__init__
    group, _, _ = child.path.rpartition("/")
    session_dir = (
        parent.session_dir / group / child.name
        if group
        else parent.session_dir / child.name
    )
    return dataclasses.replace(
        parent,
        client=client,
        session_dir=session_dir,
        child_key=child.name,
        parent_stage=parent.parent_stage or group or child.name,
    )
