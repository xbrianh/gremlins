"""Single chokepoint for agentic stage execution."""

from __future__ import annotations

import pathlib
import re
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State
from gremlins.stages.outcome import Bail

_BAIL_RE = re.compile(r"^BAIL:\s*\S+:\s*(.*)$")


def _check_bail(completed: CompletedRun) -> None:
    text = completed.text_result or ""
    last_line = next(
        (ln.strip() for ln in reversed(text.splitlines()) if ln.strip()),
        "",
    )
    m = _BAIL_RE.match(last_line)
    if m:
        raise Bail(m.group(1).strip())


async def run_agent(
    state: State,
    prompt: str,
    *,
    label: str,
    raw_path: pathlib.Path | None = None,
    model: str | None = None,
    **kw: Any,
) -> CompletedRun:
    resolved_model = model or state.stage_model or state.client.model
    completed = await state.client.run(
        prompt,
        label=label,
        model=resolved_model,
        raw_path=raw_path,
        cwd=state.worktree,
        **kw,
    )
    _check_bail(completed)
    return completed
