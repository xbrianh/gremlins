"""Implement stage for both local and gh pipelines."""

from __future__ import annotations

import logging
import pathlib
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.utils.git import (
    DivergentHead,
    EmptyImpl,
    classify_impl_outcome,
    record_pre_impl_state,
)

logger = logging.getLogger(__name__)

# Implement turns can sit silent for many minutes while the model edits files
# or runs long subagents/tools without emitting stream events. The default
# 120s STREAM_IDLE_TIMEOUT was firing spuriously on healthy implement runs;
# 600s (10 min) gives enough slack to ride out the longest observed gaps
# without masking a genuinely hung process.
IMPLEMENT_IDLE_TIMEOUT = 600.0


def _render_spec_block(spec_text: str) -> str:
    if not spec_text or not spec_text.strip():
        return ""
    trunc = ""
    if len(spec_text) > 50000:
        cut = spec_text.rfind("\n", 0, 50000)
        body = spec_text[:cut] if cut > 0 else spec_text[:50000]
        trunc = f"\n(spec truncated; {len(spec_text)} chars total)"
    else:
        body = spec_text
    return (
        "## Overarching goal (north star)\n\n"
        "This is the original chain spec. It is read-only context for\n"
        "understanding what the chain as a whole is working toward. Use\n"
        "it to make coherent local decisions while implementing the plan\n"
        "below — not as a task list. Do not expand scope beyond the\n"
        "Tasks in the plan.\n\n"
        f"~~~~\n{body}\n~~~~{trunc}\n\n"
    )


def _read_spec(session_dir: pathlib.Path) -> str:
    spec_file = session_dir / "spec.md"
    if not spec_file.exists():
        return ""
    try:
        return spec_file.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        logger.warning(
            "could not read spec.md (%s); proceeding without north-star context", exc
        )
        return ""


class Implement(Stage):
    type = "implement"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Implement:
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
    ) -> None:
        super().__init__(name, model, prompts, options)

    def run(self, state: State) -> None:
        assert state.session_dir is not None
        spec_text = _read_spec(state.session_dir)
        plan_text = (state.session_dir / "plan.md").read_text(encoding="utf-8")

        if state.issue_num:
            plan_source_label = "from the GitHub issue"
            plan_location_note = (
                "The plan lives in the GitHub issue and reviews go to PR comments; "
                "the only changes in this working tree should be product code."
            )
        else:
            plan_source_label = "below"
            plan_location_note = (
                "Reviews go to PR comments; the only changes in this working tree "
                "should be product code."
            )

        cwd_arg = str(state.worktree) if state.worktree is not None else None
        pre = record_pre_impl_state(cwd=cwd_arg)

        template = "\n\n".join(self.prompts).rstrip()
        prompt = template.format(
            spec_block=_render_spec_block(spec_text),
            plan_text=plan_text,
            plan_source_label=plan_source_label,
            plan_location_note=plan_location_note,
        )

        self.run_claude(
            prompt,
            state=state,
            label="implement",
            raw_path=state.session_dir / "stream-implement.jsonl",
            capture_events=True,
            idle_timeout=IMPLEMENT_IDLE_TIMEOUT,
        )

        outcome = classify_impl_outcome(pre, cwd=cwd_arg)
        if isinstance(outcome, EmptyImpl):
            raise RuntimeError(
                "implement produced no committed work; the agent must commit before returning"
            )
        if isinstance(outcome, DivergentHead):
            raise RuntimeError(
                f"implement diverged from pre-impl HEAD {pre.head[:7]}; expected a fast-forward"
            )
