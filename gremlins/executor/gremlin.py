"""Gremlin: pipeline orchestrator."""

from __future__ import annotations

import logging
import pathlib
import re
from collections.abc import Awaitable, Callable, Sequence
from typing import Any

from _gremlins_core.executor import StateData, write_state

logger = logging.getLogger(__name__)

_GREMLIN_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def validate_gremlin_id(gremlin_id: str) -> None:
    """Raise ValueError if gremlin_id is not a safe, non-path-traversing identifier."""
    if ".." in gremlin_id or not _GREMLIN_ID_RE.match(gremlin_id):
        raise ValueError(f"gremlin_id contains illegal characters: {gremlin_id!r}")


def write_initial_state(
    gremlin_id: str,
    kind: str,
    project_root: str,
    started_at: str,
    description: str,
    parent_id: str,
    pipeline_args: list[str],
    client_label: str,
    pipeline_path: str,
    stage_inputs: dict[str, Any],
    state_dir: pathlib.Path,
) -> None:
    """Create and persist initial state data for a gremlin."""
    validate_gremlin_id(gremlin_id)
    state_dict = {
        "id": gremlin_id,
        "kind": kind,
        "project_root": project_root,
        "workdir": "",
        "setup_kind": "worktree-detached",
        "worktree_base": "",
        "status": "running",
        "started_at": started_at,
        "description": description,
        "parent_id": parent_id,
        "pipeline_args": pipeline_args,
        "client": client_label,
        "pipeline_path": pipeline_path,
        "stage": "starting",
        "pid": None,
        "stage_inputs": stage_inputs,
        "attempt": "",
        "group_name": "",
        "child_key": "",
        "exit_code": None,
    }
    write_state(state_dir, state_dict)


def write_terminal_state(gremlin_id: str, exit_code: int) -> None:
    """Write terminal state for a completed gremlin."""
    validate_gremlin_id(gremlin_id)
    StateData(gremlin_id).write_terminal_state(exit_code)


async def run_stages(
    stages: Sequence[tuple[str, Callable[[], Awaitable[Any]]]],
    *,
    resume_from: str | None = None,
) -> None:
    start_idx = 0
    if resume_from is not None:
        names = [name for name, _ in stages]
        if resume_from not in names:
            raise ValueError(
                f"resume_from {resume_from!r} is not a valid stage; valid: {names}"
            )
        start_idx = names.index(resume_from)
    for name, fn in stages[start_idx:]:
        logger.info("running stage: %s", name)
        await fn()
