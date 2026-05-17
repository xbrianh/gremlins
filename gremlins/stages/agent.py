"""Single chokepoint for agentic stage execution."""

from __future__ import annotations

import json
import pathlib
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State, resolve_state_file
from gremlins.stages.outcome import Bail


def run_agent(
    state: State,
    prompt: str,
    *,
    label: str,
    raw_path: pathlib.Path | None = None,
    model: str | None = None,
    **kw: Any,
) -> CompletedRun:
    """Invoke the agent and check for a bail marker afterward.

    Wraps state.client.run(...) plus the bail-marker check. Raises Bail
    if the agent wrote a bail file.
    """
    extra_env: dict[str, str] = {}
    if state.data.attempt and state.data.state_file is not None:
        extra_env["GREMLIN_ATTEMPT"] = state.data.attempt
        extra_env["GREMLIN_STATE_DIR"] = str(state.data.state_file.parent)
    completed = state.client.run(
        prompt,
        label=label,
        model=model or state.client.model,
        raw_path=raw_path,
        cwd=state.worktree,
        extra_env=extra_env or None,
        **kw,
    )
    if state.data.attempt:
        sf = state.data.state_file or resolve_state_file(state.data.gremlin_id)
        if sf is not None:
            bail_path = sf.parent / f"bail_{state.data.attempt}.json"
            if bail_path.exists():
                try:
                    detail = json.loads(bail_path.read_text(encoding="utf-8")).get(
                        "detail", ""
                    )
                except Exception:
                    detail = ""
                raise Bail(detail)
    return completed
