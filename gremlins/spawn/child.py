"""Subprocess entry point: execute one pipeline stage in a fresh process.

Spec file schema (JSON):
    {
        "stage_dict":      <dict>       parsed YAML dict; passed to parse_stage()
        "client":          <str>        "provider:model"
        "child_id":        <str>        gremlin id for this child; state.json loaded from state_root/<child_id>/
        "parent_id":       <str|null>   parent gremlin id (informational)
        "group_name":      <str|null>   parallel group name (informational)
        "worktree":        <str|null>   absolute path to git worktree or null
        "worktree_parent": <str|null>   absolute path to worktree parent or null
        "pipeline_path":   <str|null>   absolute path to pipeline YAML or null
        "child_key":       <str|null>   parallel group child identifier or null
        "attempt":         <str|null>   attempt id for this child (overrides state.json)
        "parent_stage":    <str>        parent stage name for sub-stage tracking
        "repo":            <str>        "owner/repo" for gh API calls (from parent)
        "base_ref":        <str|null>   base branch/ref name for prompt template substitution (e.g. "main"); omitted or null → ""
    }

Result file schema (written to <spec_path>.result):
    {
        "status":     "done" | "bail" | "error"
        "detail":     <str>        reason for bail / error
        "returncode": <int|null>   always null
        "cost_usd":   <float>      total client cost accumulated during the stage
    }

Usage:
    python -m gremlins.spawn.child <spec_path>
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import pathlib
import sys
import traceback
from typing import Any, cast

from _gremlins_core.schemas import parse_stage
from _gremlins_core.stages import Bail

from gremlins.executor.gremlin import Gremlin
from gremlins.logging_setup import configure_logging

logger = logging.getLogger(__name__)


def _load_spec(spec_path: pathlib.Path) -> dict[str, Any]:
    if not spec_path.exists():
        raise FileNotFoundError(f"spec file not found: {spec_path}")
    parsed: Any = json.loads(spec_path.read_text(encoding="utf-8"))
    if not isinstance(parsed, dict):
        raise ValueError(f"spec must be a JSON object, got {type(parsed).__name__}")
    return cast(dict[str, Any], parsed)


def _write_result(result_path: pathlib.Path, payload: dict[str, Any]) -> None:
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(json.dumps(payload), encoding="utf-8")


def _try_write_terminal(gremlin: Gremlin, rc: int) -> None:
    try:
        state = gremlin.state
        if state is not None:
            state.data.write_terminal_state(rc)
    except Exception:
        logger.warning("write_terminal_state failed", exc_info=True)


async def _run(spec_path: pathlib.Path) -> int:
    spec = _load_spec(spec_path)
    result_path = pathlib.Path(str(spec_path) + ".result")

    stage_dict = spec.get("stage_dict")
    if not isinstance(stage_dict, dict):
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": "spec missing required 'stage_dict' field",
                "returncode": None,
                "cost_usd": 0.0,
            },
        )
        return 2

    try:
        stage = parse_stage(stage_dict)
        gremlin = Gremlin.from_subprocess(spec)
    except Exception as exc:
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": str(exc),
                "returncode": None,
                "cost_usd": 0.0,
            },
        )
        return 2

    if gremlin.gremlin_id:
        try:
            gremlin.patch_state_for(gremlin.gremlin_id, pid=os.getpid())
        except Exception:
            logger.warning("Failed to record PID in child state", exc_info=True)

    if gremlin.bootstrap_cmds and gremlin.worktree_dir:
        from gremlins.executor.bootstrap import run_bootstrap as _run_bootstrap

        try:
            await _run_bootstrap(gremlin.bootstrap_cmds, gremlin.worktree_dir)
        except Exception as exc:
            _write_result(
                result_path,
                {
                    "status": "error",
                    "detail": f"bootstrap failed: {exc}",
                    "returncode": None,
                    "cost_usd": 0.0,
                },
            )
            return 2

    state = gremlin.state
    if state is None:
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": "gremlin state not initialized",
                "returncode": None,
                "cost_usd": 0.0,
            },
        )
        return 2
    if stage.client is None:
        stage.client = state.client

    # Subprocess children write to their own state.json (state_root/<child_id>/),
    # and their branch pipeline only contains this single child stage.
    # Writing the child name as the top-level `stage` is therefore the correct
    # resume target (unlike the in-process make_runner path that passes
    # parent_stage because it writes to the parent's state.json).
    state.record_stage_progress(stage.name)

    try:
        await stage.run(gremlin)
    except Bail as b:
        cost = getattr(state.client, "total_cost_usd", 0.0) or 0.0
        _write_result(
            result_path,
            {
                "status": "bail",
                "detail": b.reason,
                "returncode": None,
                "cost_usd": cost,
            },
        )
        _try_write_terminal(gremlin, 1)
        return 1
    except Exception as exc:
        cost = getattr(state.client, "total_cost_usd", 0.0) or 0.0
        _write_result(
            result_path,
            {
                "status": "error",
                "detail": str(exc),
                "returncode": None,
                "cost_usd": cost,
            },
        )
        traceback.print_exc()
        _try_write_terminal(gremlin, 2)
        return 2

    cost = getattr(state.client, "total_cost_usd", 0.0) or 0.0
    _write_result(
        result_path,
        {"status": "done", "detail": "", "returncode": None, "cost_usd": cost},
    )
    _try_write_terminal(gremlin, 0)
    return 0


def main(argv: list[str] | None = None) -> int:
    configure_logging()
    if argv is None:
        argv = sys.argv[1:]
    if len(argv) != 1:
        sys.stderr.write("run_child: usage: <spec_path>\n")
        return 1

    spec_path = pathlib.Path(argv[0])
    try:
        return asyncio.run(_run(spec_path))
    except (FileNotFoundError, ValueError) as exc:
        sys.stderr.write(f"run_child: {exc}\n")
        return 1
    except Exception:
        traceback.print_exc()
        return 2


if __name__ == "__main__":
    sys.exit(main())
