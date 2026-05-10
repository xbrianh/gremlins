"""Implement stage for both local and gh pipelines."""

from __future__ import annotations

import json
import os
import pathlib
from typing import Any

from gremlins.git import (
    DirtyOnly,
    DivergentHead,
    EmptyImpl,
    classify_impl_outcome,
    record_pre_impl_state,
)
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages import Stage, register_stage
from gremlins.state import patch_state, pipeline_uses_gh, resolve_state_file

# Implement turns can sit silent for many minutes while the model edits files
# or runs long subagents/tools without emitting stream events. The default
# 120s STREAM_IDLE_TIMEOUT was firing spuriously on healthy implement runs;
# 600s (10 min) gives enough slack to ride out the longest observed gaps
# without masking a genuinely hung process.
IMPLEMENT_IDLE_TIMEOUT = 600.0


def changes_outside_git(sentinel: pathlib.Path, session_dir: pathlib.Path) -> bool:
    try:
        threshold = sentinel.stat().st_mtime
    except Exception:
        return False
    cwd = pathlib.Path(".").resolve()
    try:
        session_resolved = session_dir.resolve()
    except Exception:
        session_resolved = session_dir
    for dirpath, dirnames, filenames in os.walk(cwd):
        dirnames[:] = [d for d in dirnames if d != ".git"]
        dp = pathlib.Path(dirpath)
        try:
            dp_resolved = dp.resolve()
            if (
                dp_resolved == session_resolved
                or session_resolved in dp_resolved.parents
            ):
                dirnames[:] = []
                continue
        except Exception:
            pass
        for f in filenames:
            fp = dp / f
            try:
                if fp.stat().st_mtime > threshold:
                    return True
            except Exception:
                continue
    return False


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


class Implement(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        is_git: bool,
        spec_text: str = "",
        cwd: str | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.is_git = is_git
        self.spec_text = spec_text
        self._cwd = cwd

    @property
    def _impl_cwd(self) -> str | None:
        return self._cwd or (
            str(self.state.worktree) if self.state.worktree is not None else None
        )

    def run(self, pipe: Any) -> None:
        pipeline_data = getattr(pipe, "pipeline_data", None)
        if pipeline_data is not None and pipeline_uses_gh(pipeline_data):
            self._run_gh(pipe)
        else:
            self._run_local()

    def _run_local(self) -> None:
        cwd_arg = str(self.state.worktree) if self.state.worktree is not None else None
        pre = None
        pre_sentinel: pathlib.Path | None = None
        if self.is_git:
            pre = record_pre_impl_state(cwd=cwd_arg)
        else:
            pre_sentinel = self.state.session_dir / ".pre-impl"
            pre_sentinel.touch()

        impl_commit_instr = ""
        if self.is_git:
            impl_commit_instr = (
                (BUNDLED_PROMPT_DIR / "impl_commit_git.md")
                .read_text(encoding="utf-8")
                .rstrip()
            )

        plan_text = (self.state.session_dir / "plan.md").read_text(encoding="utf-8")
        template = "\n\n".join(self.prompts).rstrip()
        prompt = template.format(
            spec_block=_render_spec_block(self.spec_text),
            plan_text=plan_text,
            impl_commit_instr=impl_commit_instr,
        )
        self.run_claude(
            prompt,
            label="implement",
            raw_path=self.state.session_dir / "stream-implement.jsonl",
            idle_timeout=IMPLEMENT_IDLE_TIMEOUT,
        )

        if self.is_git:
            if pre is None:
                raise RuntimeError("pre-impl state not captured")
            outcome = classify_impl_outcome(pre, cwd=cwd_arg)
            if isinstance(outcome, DirtyOnly):
                raise RuntimeError(
                    "implement left uncommitted changes; the agent must commit before returning"
                )
            if isinstance(outcome, EmptyImpl):
                raise RuntimeError("implement produced no work")
            if isinstance(outcome, DivergentHead):
                raise RuntimeError(
                    f"implement diverged from pre-impl HEAD {pre.head[:7]}; expected a fast-forward"
                )
        else:
            if pre_sentinel is None:
                raise RuntimeError("pre-impl sentinel not created")
            if not changes_outside_git(pre_sentinel, self.state.session_dir):
                raise RuntimeError("implementation stage produced no changes; aborting")

    def _run_gh(self, pipe: Any) -> None:
        state_file = resolve_state_file(self.state.gr_id)
        issue_num = ""
        if state_file and state_file.exists():
            try:
                issue_num = (
                    json.loads(state_file.read_text(encoding="utf-8")).get("issue_num")
                    or ""
                )
            except (OSError, ValueError, KeyError):
                pass

        if issue_num:
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

        pre = record_pre_impl_state(cwd=self._impl_cwd)
        pipe.impl_pre_state = pre
        if self.state.gr_id:
            patch_state(
                self.state.gr_id, impl_pre_head=pre.head, impl_pre_branch=pre.branch
            )

        plan_text = (self.state.session_dir / "plan.md").read_text(encoding="utf-8")
        template = "\n\n".join(self.prompts).rstrip()
        prompt = template.format(
            spec_block=_render_spec_block(self.spec_text),
            plan_source_label=plan_source_label,
            issue_body=plan_text,
            plan_location_note=plan_location_note,
        )

        self.run_claude(
            prompt,
            label="implement",
            raw_path=self.state.session_dir / "stream-implement.jsonl",
            capture_events=True,
            idle_timeout=IMPLEMENT_IDLE_TIMEOUT,
        )

        outcome = classify_impl_outcome(pre, cwd=self._impl_cwd)
        if isinstance(outcome, DirtyOnly):
            raise RuntimeError(
                "implement left uncommitted changes; the agent must commit before returning"
            )
        if isinstance(outcome, EmptyImpl):
            raise RuntimeError("implement produced no work")
        if isinstance(outcome, DivergentHead):
            raise RuntimeError(
                f"implement diverged from pre-impl HEAD {pre.head[:7]}; expected a fast-forward"
            )


register_stage("implement", Implement)
