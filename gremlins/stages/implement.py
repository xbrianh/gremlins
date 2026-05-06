"""Implement stage for both local and gh pipelines."""

from __future__ import annotations

import dataclasses
import os
import pathlib
import subprocess
import sys

from gremlins.git import (
    DivergentHead,
    EmptyImpl,
    HeadAdvanced,
    ImplOutcome,
    PreImplState,
    classify_impl_outcome,
    create_handoff_branch,
    git_head,
    git_head_of_workdir,
    record_pre_impl_state,
    reset_pre_branch,
    sweep_stale_handoff_branches,
)
from gremlins.stages.context import StageContext
from gremlins.stages.registry import register_stage

PROMPT_LOCAL_PATH = (
    pathlib.Path(__file__).resolve().parent.parent
    / "pipelines"
    / "prompts"
    / "implement_local.md"
)
PROMPT_GH_PATH = (
    pathlib.Path(__file__).resolve().parent.parent
    / "pipelines"
    / "prompts"
    / "implement_gh.md"
)


@dataclasses.dataclass
class ImplStageResult:
    """Returned by ``run`` when ``options.kind='gh'``."""

    pre_state: PreImplState
    outcome: ImplOutcome
    handoff_branch: str  # empty string when outcome is DirtyOnly (no branch created)


@dataclasses.dataclass
class ImplementOptions:
    impl_model: str | None
    plan_text: str
    code_style: str
    is_git: bool
    kind: str = "local"
    spec_text: str = ""
    issue_num: str = ""
    cwd: str | None = None
    prompt_path: pathlib.Path | None = None


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


def run(ctx: StageContext, options: ImplementOptions) -> ImplStageResult | None:
    """Run the implement stage.

    Returns None for kind='local'. Returns ImplStageResult for kind='gh'.
    """
    if options.kind == "gh":
        return _run_implement_gh(ctx, options)

    # --- local path ---
    pre_head = ""
    pre_sentinel: pathlib.Path | None = None
    cwd_arg = str(ctx.worktree) if ctx.worktree is not None else None
    if options.is_git:
        pre_head = git_head_of_workdir(cwd_arg) if cwd_arg is not None else git_head()
    else:
        pre_sentinel = ctx.session_dir / ".pre-impl"
        pre_sentinel.touch()

    impl_commit_instr = "."
    if options.is_git:
        impl_commit_instr = (
            ", stage the changed files by name and create a single git commit "
            "with a clear message that references the implementation plan "
            "(refer to it as `plan.md` in the commit message, not by absolute "
            "path). Do NOT create any meta/scaffolding files in the repo — no "
            "`.claude-workflow/` directory, no `plan.md`, no review docs, no "
            "notes-to-self. Do not push."
        )

    template = (options.prompt_path or PROMPT_LOCAL_PATH).read_text(encoding="utf-8")
    prompt = template.format(
        code_style=options.code_style,
        spec_block=_render_spec_block(options.spec_text),
        plan_text=options.plan_text,
        impl_commit_instr=impl_commit_instr,
    )
    ctx.client.run(
        prompt,
        label="implement",
        model=options.impl_model,
        raw_path=ctx.session_dir / "stream-implement.jsonl",
        cwd=ctx.worktree,
    )

    if options.is_git:
        post_head = git_head_of_workdir(cwd_arg) if cwd_arg is not None else git_head()
        porcelain = subprocess.run(
            ["git", "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
            cwd=cwd_arg,
        )
        if post_head == pre_head and not porcelain.stdout.strip():
            raise RuntimeError("implementation stage produced no changes; aborting")
    else:
        assert pre_sentinel is not None
        if not changes_outside_git(pre_sentinel, ctx.session_dir):
            raise RuntimeError("implementation stage produced no changes; aborting")

    return None


def _run_implement_gh(ctx: StageContext, options: ImplementOptions) -> ImplStageResult:
    """gh-specific implement: run claude, then orchestrate the handoff branch lifecycle."""
    if options.issue_num:
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

    template = (options.prompt_path or PROMPT_GH_PATH).read_text(encoding="utf-8")
    prompt = template.format(
        code_style=options.code_style,
        spec_block=_render_spec_block(options.spec_text),
        plan_source_label=plan_source_label,
        issue_body=options.plan_text,
        plan_location_note=plan_location_note,
    )

    impl_cwd = options.cwd or (str(ctx.worktree) if ctx.worktree is not None else None)
    pre_state = record_pre_impl_state(cwd=impl_cwd)

    ctx.client.run(
        prompt,
        label="implement",
        model=options.impl_model,
        raw_path=ctx.session_dir / "stream-implement.jsonl",
        capture_events=True,
        cwd=ctx.worktree,
    )

    outcome = classify_impl_outcome(pre_state, cwd=impl_cwd)

    if isinstance(outcome, EmptyImpl):
        raise RuntimeError(
            "implementation step produced no changes; refusing to open empty PR"
        )
    if isinstance(outcome, DivergentHead):
        raise RuntimeError(
            f"implementation changed HEAD from {outcome.pre_head} to {outcome.post_head} "
            "without advancing from the starting commit; refusing to treat this as "
            "committed work to hand off"
        )

    handoff_branch = ""
    if isinstance(outcome, HeadAdvanced):
        handoff_branch = create_handoff_branch(pre_state, cwd=impl_cwd)
        reset_pre_branch(pre_state, cwd=impl_cwd)
        sweep_stale_handoff_branches(handoff_branch, cwd=impl_cwd)
        commit_count = outcome.commit_count
        pre_branch_note = f" and reset {pre_state.branch}" if pre_state.branch else ""
        sys.stdout.write(
            f"    implement committed during run; moved {commit_count} commit(s) "
            f"onto {handoff_branch}{pre_branch_note}\n"
        )
        sys.stdout.flush()

    return ImplStageResult(
        pre_state=pre_state,
        outcome=outcome,
        handoff_branch=handoff_branch,
    )


register_stage("implement", run)
