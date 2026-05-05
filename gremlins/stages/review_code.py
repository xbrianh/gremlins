"""Code review stage."""

from __future__ import annotations

import dataclasses
import pathlib

from gremlins.clients.protocol import ClaudeClient
from gremlins.prompts import load_prompts
from gremlins.stages.context import StageContext
from gremlins.stages.registry import register_stage
from gremlins.state import emit_bail, set_stage


def _run_reviewer(
    *,
    client: ClaudeClient,
    model: str,
    out_file: pathlib.Path,
    focus: str,
    context: str,
    code_style: str,
    where_field: str,
    label: str,
    raw_path: pathlib.Path,
) -> None:
    style_preamble = ""
    if code_style:
        style_preamble = (
            "Follow these coding-style rules and flag violations — long functions, "
            "inheritance where functions suffice, dead comments, speculative "
            "abstractions — alongside the correctness, security, and performance "
            "findings in the focus section below:\n\n"
            f"{code_style}\n\n"
        )
    prompt = f"""{style_preamble}Read surrounding code as needed — don't review in isolation.

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
    client.run(prompt, label=label, model=model, raw_path=raw_path)


@dataclasses.dataclass
class ReviewCodeOptions:
    plan_text: str
    is_git: bool
    code_style: str
    model: str
    stage_name: str
    prompt_paths: list[pathlib.Path]


def run(ctx: StageContext, options: ReviewCodeOptions) -> pathlib.Path:
    out_file = ctx.session_dir / f"{options.stage_name}-{options.model}.md"

    for stale in ctx.session_dir.glob(f"{options.stage_name}-*.md"):
        try:
            stale.unlink()
        except OSError:
            pass

    focus = load_prompts(options.prompt_paths)
    if not focus.strip():
        raise ValueError(
            f"stage '{options.stage_name}': prompt_paths produced empty focus; "
            "check that prompt_paths is non-empty and all files have content"
        )

    if options.is_git:
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
    if options.plan_text:
        code_context = (
            f"The plan for this change is:\n\n{options.plan_text}\n\n{code_scope}"
        )
    else:
        code_context = code_scope

    try:
        set_stage(
            ctx.gr_id, options.stage_name, {"model": f"running ({options.model})"}
        )
        _run_reviewer(
            client=ctx.client,
            model=options.model,
            out_file=out_file,
            focus=focus,
            context=code_context,
            code_style=options.code_style,
            where_field="**File:** `path/to/file.ext:<line>`",
            label=f"{options.stage_name}:{options.model}",
            raw_path=ctx.session_dir
            / f"stream-{options.stage_name}-{options.model}.jsonl",
        )
        set_stage(ctx.gr_id, options.stage_name, {"model": f"done ({options.model})"})
        if not out_file.exists() or out_file.stat().st_size == 0:
            raise RuntimeError(f"review {options.model} did not produce {out_file}")
    except (SystemExit, Exception) as exc:
        emit_bail(
            ctx.gr_id,
            "other",
            f"{options.stage_name} stage failed: {exc}"[:200],
            child_key=ctx.child_key,
        )
        raise

    return out_file


register_stage("review-code", run)
