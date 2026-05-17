"""Code review stage."""

from __future__ import annotations

import logging
import pathlib
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.agent import bail_command, run_agent
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome

logger = logging.getLogger(__name__)


def _run_reviewer(
    *,
    state: State,
    model: str,
    out_file: pathlib.Path,
    focus: str,
    context: str,
    where_field: str,
    label: str,
    raw_path: pathlib.Path,
) -> None:
    prompt = f"""Read surrounding code as needed — don't review in isolation.

{context}

Structure your review as markdown:

# Review ({model})

## Summary
2-4 sentences overall.

## Findings
For each actionable finding:
### <short title>
- {where_field}
- **Severity:** blocker | major | minor | nit
- **What:** what's wrong
- **Fix:** concrete suggestion

If there are no issues worth raising, write a Findings section that says so explicitly.

Do NOT make any code changes — only write the review file.

{focus}

`{out_file}` is the canonical and required location for your review output in every case, including any short-circuit one-liner the prompt tells you to emit. Do not emit the verdict only to chat; write it to `{out_file}` and then stop."""
    run_agent(state, prompt, label=label, model=model, raw_path=raw_path)


class ReviewCode(Stage):
    type = "review-code"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options

    def run(self, state: State) -> Outcome:
        model = state.stage_model or state.client.model
        if not model:
            raise ValueError(f"stage {self.name!r}: model must be set")
        out_file = state.session_dir / f"{self.name}-{model}.md"

        for stale in state.session_dir.glob(f"{self.name}-*.md"):
            try:
                stale.unlink()
            except OSError:
                pass

        focus = "\n\n".join(self.prompts).rstrip()
        if not focus.strip():
            raise ValueError(
                f"stage '{self.name}': prompts produced empty focus; "
                "check that prompts is non-empty and all entries have content"
            )

        plan_file = state.session_dir / "plan.md"
        plan_text = plan_file.read_text(encoding="utf-8") if plan_file.exists() else ""

        code_scope = (
            "Review the changes introduced by the most recent commit "
            "(HEAD vs HEAD~1) plus any uncommitted working-tree changes. "
            "Use `git diff HEAD~1 HEAD` and `git diff` to see the scope."
        )
        if plan_text:
            code_context = (
                f"The plan for this change is:\n\n{plan_text}\n\n{code_scope}"
            )
        else:
            code_context = code_scope

        state.record_stage_progress(
            self.name, {"model": f"running ({model})"}, parent_stage=state.parent_stage
        )
        _run_reviewer(
            state=state,
            model=model,
            out_file=out_file,
            focus=focus,
            context=code_context,
            where_field="**File:** `path/to/file.ext:<line>`",
            label=f"{self.name}:{model}",
            raw_path=state.session_dir / f"stream-{self.name}-{model}.jsonl",
        )
        state.record_stage_progress(
            self.name, {"model": f"done ({model})"}, parent_stage=state.parent_stage
        )
        logger.info("code review (%s): %s", model, out_file)
        if not out_file.exists() or out_file.stat().st_size == 0:
            raise RuntimeError(f"review {model} did not produce {out_file}")

        return Done()


class GitHubReviewPullRequest(Stage):
    type = "github-review-pull-request"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> GitHubReviewPullRequest:
        from gremlins.pipeline.loader import get_client_from_dict

        prompts: list[str] = d.get("prompt") or []
        if not prompts:
            raise ValueError(
                f"stage {d['name']!r}: 'prompt' is required for github-review-pull-request"
            )
        stage = cls(d["name"], prompts, d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_url: str = "",
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.pr_url = pr_url

    def run(self, state: State) -> Outcome:
        pr_url = self.pr_url or state.data.read_pr_url()
        if not pr_url:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        prompt = (
            "\n\n".join(self.prompts)
            .rstrip()
            .format(
                bail_command=bail_command(state),
                pr_url=pr_url,
            )
        )
        run_agent(
            state,
            prompt,
            label="github-review-pull-request",
            raw_path=state.session_dir / "stream-github-review-pull-request.jsonl",
        )
        return Done()
