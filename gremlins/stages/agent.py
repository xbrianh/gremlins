"""Single chokepoint for agentic stage execution."""

from __future__ import annotations

import asyncio
import json
import pathlib
import shlex
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State, resolve_state_file
from gremlins.stages.outcome import Bail


def _read_bail_detail(bail_path: pathlib.Path) -> str:
    try:
        return json.loads(bail_path.read_text(encoding="utf-8")).get("detail", "")
    except Exception:
        return ""


def _check_bail(state: State) -> None:
    if not state.data.attempt:
        return
    sf = state.data.state_file or resolve_state_file(state.data.gremlin_id)
    if sf is None:
        return
    bail_path = sf.parent / f"bail_{state.data.attempt}.json"
    if bail_path.exists():
        raise Bail(_read_bail_detail(bail_path))


def bail_command(state: State) -> str:
    script = (
        "import sys,json,os,pathlib; "
        "d=pathlib.Path(os.environ['GREMLIN_STATE_DIR']); "
        "a=os.environ['GREMLIN_ATTEMPT']; "
        "p=d/f'bail_{a}.json'; "
        "p.exists() or p.write_text(json.dumps({'class':sys.argv[1],'detail':sys.argv[2] if len(sys.argv)>2 else ''}))"
    )
    return f"python -c {shlex.quote(script)}"


def run_agent(
    state: State,
    prompt: str,
    *,
    label: str,
    raw_path: pathlib.Path | None = None,
    model: str | None = None,
    **kw: Any,
) -> CompletedRun:
    """Invoke the agent, inject bail-marker env, and raise Bail if the marker is set."""
    extra_env: dict[str, str] = {}
    sf = state.data.state_file or resolve_state_file(state.data.gremlin_id)
    if state.data.attempt and sf is not None:
        extra_env["GREMLIN_ATTEMPT"] = state.data.attempt
        extra_env["GREMLIN_STATE_DIR"] = str(sf.parent)
    resolved_model = model or state.stage_model or state.client.model
    completed = asyncio.run(
        state.client.run(
            prompt,
            label=label,
            model=resolved_model,
            raw_path=raw_path,
            cwd=state.worktree,
            extra_env=extra_env or None,
            **kw,
        )
    )
    _check_bail(state)
    return completed
