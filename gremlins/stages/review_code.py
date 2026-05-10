"""Code review stage."""

from __future__ import annotations

import logging
import pathlib
from typing import Any

from gremlins.clients.protocol import ClaudeClient
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import (
    check_bail,
    emit_bail,
    pipeline_uses_gh,
    read_pr_url,
    set_stage,
)

logger = logging.getLogger(__name__)


def _run_reviewer(
    *,
    client: ClaudeClient,
    model: str,
    out_file: pathlib.Path,
    focus: str,
    context: str,
    where_field: str,
    label: str,
    raw_path: pathlib.Path,
    cwd: pathlib.Path | None = None,
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
    client.run(prompt, label=label, model=model, raw_path=raw_path, cwd=cwd)


class ReviewCode(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        plan_text: str,
        is_git: bool,
        pr_url: str = "",
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.plan_text = plan_text
        self.is_git = is_git
        self.pr_url = pr_url

    def run(self, pipe: Any) -> Any:
        if pipe is not None and pipeline_uses_gh(pipe.pipeline_data):
            return self.results_to_github(pipe)
        return self.results_to_local(pipe)

    def results_to_local(self, pipe: Any) -> pathlib.Path:
        if self.model is None:
            raise ValueError(f"stage {self.name!r}: model must be set")
        out_file = self.state.session_dir / f"{self.name}-{self.model}.md"

        for stale in self.state.session_dir.glob(f"{self.name}-*.md"):
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

        if self.is_git:
            code_scope = (
                "Review the changes introduced by the most recent commit "
                "(HEAD vs HEAD~1) plus any uncommitted working-tree changes. "
                "Use `git diff HEAD~1 HEAD` and `git diff` to see the scope."
            )
        else:
            code_scope = (
                "Review the uncommitted changes in this directory (`git diff` if "
                "available, otherwise inspect recently modified files)."
            )
        if self.plan_text:
            code_context = (
                f"The plan for this change is:\n\n{self.plan_text}\n\n{code_scope}"
            )
        else:
            code_context = code_scope

        try:
            set_stage(self.state.gr_id, self.name, {"model": f"running ({self.model})"})
            _run_reviewer(
                client=self.state.client,
                model=self.model,
                out_file=out_file,
                focus=focus,
                context=code_context,
                where_field="**File:** `path/to/file.ext:<line>`",
                label=f"{self.name}:{self.model}",
                raw_path=self.state.session_dir
                / f"stream-{self.name}-{self.model}.jsonl",
                cwd=self.state.worktree,
            )
            set_stage(self.state.gr_id, self.name, {"model": f"done ({self.model})"})
            logger.info("code review (%s): %s", self.model, out_file)
            if not out_file.exists() or out_file.stat().st_size == 0:
                raise RuntimeError(f"review {self.model} did not produce {out_file}")
        except (SystemExit, Exception) as exc:
            emit_bail(
                self.state.gr_id,
                "other",
                f"{self.name} stage failed: {exc}"[:200],
                child_key=self.state.child_key,
            )
            raise

        return out_file

    def results_to_github(self, pipe: Any) -> None:
        pr_url = self.pr_url or read_pr_url(self.state.gr_id)
        if not pr_url:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        prompt = (
            "\n\n".join(self.prompts)
            .rstrip()
            .format(
                bail_command=self.bail_command(),
                pr_url=pr_url,
            )
        )
        self.run_claude(
            prompt,
            label="ghreview",
            raw_path=self.state.session_dir / "stream-ghreview.jsonl",
        )
        check_bail(self.state.gr_id, "/ghreview", child_key=self.state.child_key)


register_stage("review-code", ReviewCode)
register_stage("ghreview", ReviewCode)
