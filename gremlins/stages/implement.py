"""Implement stage for both local and gh pipelines.

For ``kind='local'``: renders ``implement_local.md``, runs claude, enforces the
empty-implementation invariant (spec: an empty impl must never flow into review).

For ``kind='gh'``: renders ``implement_gh.md``, runs claude, then runs the
impl-handoff branch lifecycle from ``gremlins/git.py``.  Returns an
``ImplStageResult`` with the pre-impl state snapshot and classified outcome.
Raises on ``DivergentHead`` or ``EmptyImpl``.
"""

from __future__ import annotations

import dataclasses
import os
import pathlib
import subprocess
import sys

from ..clients.claude import ClaudeClient
from ..git import (
    DivergentHead,
    EmptyImpl,
    HeadAdvanced,
    ImplOutcome,
    PreImplState,
    classify_impl_outcome,
    create_handoff_branch,
    git_head,
    record_pre_impl_state,
    reset_pre_branch,
    sweep_stale_handoff_branches,
)

PROMPT_LOCAL_PATH = (
    pathlib.Path(__file__).resolve().parent.parent / "prompts" / "implement_local.md"
)
PROMPT_GH_PATH = (
    pathlib.Path(__file__).resolve().parent.parent / "prompts" / "implement_gh.md"
)


@dataclasses.dataclass
class ImplStageResult:
    """Returned by ``run_implement_stage`` when ``kind='gh'``."""

    pre_state: PreImplState
    outcome: ImplOutcome
    handoff_branch: str  # empty string when outcome is DirtyOnly (no branch created)


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


def run_implement_stage(
    *,
    client: ClaudeClient,
    impl_model: str | None,
    plan_text: str,
    code_style: str,
    session_dir: pathlib.Path,
    is_git: bool,
    kind: str = "local",
    spec_text: str = "",
    # local-only (kept for API compatibility, not used in function body)
    plan_file: pathlib.Path | None = None,
    # gh-only
    issue_num: str = "",
    cwd: str | None = None,
) -> ImplStageResult | None:
    """Run the implement stage.

    Returns ``None`` for ``kind='local'``.  Returns ``ImplStageResult`` for
    ``kind='gh'`` so the orchestrator can thread the session_id into commit-pr.
    """
    if kind == "gh":
        return _run_implement_gh(
            client=client,
            impl_model=impl_model,
            plan_text=plan_text,
            code_style=code_style,
            session_dir=session_dir,
            issue_num=issue_num,
            cwd=cwd,
            spec_text=spec_text,
        )

    # --- local path ---
    pre_head = ""
    pre_sentinel: pathlib.Path | None = None
    if is_git:
        pre_head = git_head()
    else:
        pre_sentinel = session_dir / ".pre-impl"
        pre_sentinel.touch()

    impl_commit_instr = "."
    if is_git:
        impl_commit_instr = (
            ", stage the changed files by name and create a single git commit "
            "with a clear message that references the implementation plan "
            "(refer to it as `plan.md` in the commit message, not by absolute "
            "path). Do NOT create any meta/scaffolding files in the repo — no "
            "`.claude-workflow/` directory, no `plan.md`, no review docs, no "
            "notes-to-self. Do not push."
        )

    template = PROMPT_LOCAL_PATH.read_text(encoding="utf-8")
    prompt = template.format(
        code_style=code_style,
        spec_block=_render_spec_block(spec_text),
        plan_text=plan_text,
        impl_commit_instr=impl_commit_instr,
    )
    client.run(
        prompt,
        label="implement",
        model=impl_model,
        raw_path=session_dir / "stream-implement.jsonl",
    )

    if is_git:
        post_head = git_head()
        porcelain = subprocess.run(
            ["git", "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        if post_head == pre_head and not porcelain.stdout.strip():
            raise RuntimeError("implementation stage produced no changes; aborting")
    else:
        assert pre_sentinel is not None
        if not changes_outside_git(pre_sentinel, session_dir):
            raise RuntimeError("implementation stage produced no changes; aborting")

    return None


def _run_implement_gh(
    *,
    client: ClaudeClient,
    impl_model: str | None,
    plan_text: str,
    code_style: str,
    session_dir: pathlib.Path,
    issue_num: str,
    cwd: str | None,
    spec_text: str = "",
) -> ImplStageResult:
    """gh-specific implement: run claude, then orchestrate the handoff branch lifecycle."""
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

    template = PROMPT_GH_PATH.read_text(encoding="utf-8")
    prompt = template.format(
        code_style=code_style,
        spec_block=_render_spec_block(spec_text),
        plan_source_label=plan_source_label,
        issue_body=plan_text,
        plan_location_note=plan_location_note,
    )

    pre_state = record_pre_impl_state(cwd=cwd)

    client.run(
        prompt,
        label="implement",
        model=impl_model,
        raw_path=session_dir / "stream-implement.jsonl",
        capture_events=True,
    )

    outcome = classify_impl_outcome(pre_state, cwd=cwd)

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
        handoff_branch = create_handoff_branch(pre_state, cwd=cwd)
        reset_pre_branch(pre_state, cwd=cwd)
        sweep_stale_handoff_branches(handoff_branch, cwd=cwd)
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
