"""Single chokepoint for agentic stage execution."""

from __future__ import annotations

import logging
import pathlib
import re
from typing import TYPE_CHECKING, Any

from _gremlins_core.clients import PyCompletedRun as CompletedRun
from _gremlins_core.stages import Bail

if TYPE_CHECKING:
    from gremlins.executor.gremlin import State

logger = logging.getLogger(__name__)

_BAIL_RE = re.compile(r"^BAIL:\s*\S+:\s*(.*)$")


def _record_token_usage(state: State, completed: CompletedRun) -> None:
    usage = completed.token_usage
    if usage is None:
        return
    delta = {
        "prompt_tokens": int(usage.prompt_tokens),
        "completion_tokens": int(usage.completion_tokens),
        "cached_input_tokens": int(usage.cached_input_tokens),
        "cache_creation_input_tokens": int(usage.cache_creation_input_tokens),
        "reasoning_tokens": int(usage.reasoning_tokens),
        "turns": int(usage.turns),
    }
    state.data.accumulate_token_usage(delta)
    logger.info(
        "token usage: prompt=%d completion=%d cached=%d cache_creation=%d reasoning=%d turns=%d",
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.cached_input_tokens,
        usage.cache_creation_input_tokens,
        usage.reasoning_tokens,
        usage.turns,
    )


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
    resolved_model = model or state.client.model
    completed = await state.client.run(
        prompt,
        label=label,
        model=resolved_model,
        raw_path=str(raw_path) if raw_path is not None else None,
        cwd=str(state.worktree) if state.worktree is not None else None,
        artifact_dir=str(state.artifact_dir),
        **kw,
    )
    _record_token_usage(state, completed)
    _check_bail(completed)
    return completed
