"""Commit-and-open-PR stage for the gh pipeline.

Selects the correct action clause from ``gremlins/prompts/`` based on the
implement stage's classified outcome, gathers the diff from the impl-handoff
branch, assembles the full commit-pr prompt, and runs a fresh claude session
(no ``--resume``).  All context comes from disk state so the stage can resume
cleanly without depending on an in-memory session_id.
"""

from __future__ import annotations

import pathlib
import subprocess

from ..clients.claude import ClaudeClient, CompletedRun
from ..gh_utils import extract_gh_url
from ..git import HeadAdvanced, ImplOutcome

PROMPTS_DIR = pathlib.Path(__file__).resolve().parent.parent / "prompts"


def _load(name: str) -> str:
    return (PROMPTS_DIR / name).read_text(encoding="utf-8")


def _get_diff(
    outcome: ImplOutcome,
    impl_handoff_branch: str,
    base_ref: str,
    cwd: str | None,
) -> str:
    run_kw: dict = dict(capture_output=True, text=True, check=False)
    if cwd:
        run_kw["cwd"] = cwd
    if isinstance(outcome, HeadAdvanced):
        r = subprocess.run(
            ["git", "log", "--patch", f"{base_ref}..{impl_handoff_branch}"],
            **run_kw,
        )
        if r.returncode != 0:
            raise RuntimeError(
                f"git log --patch {base_ref}..{impl_handoff_branch} failed "
                f"(rc={r.returncode}): {r.stderr.strip()}"
            )
        diff = r.stdout.strip()
    else:
        # DirtyOnly: uncommitted changes relative to HEAD
        r = subprocess.run(["git", "diff", "HEAD"], **run_kw)
        if r.returncode != 0:
            raise RuntimeError(
                f"git diff HEAD failed (rc={r.returncode}): {r.stderr.strip()}"
            )
        diff = r.stdout.strip()
    return diff or "(no diff available)"


def run_commit_pr_stage(
    *,
    client: ClaudeClient,
    model: str | None,
    impl_outcome: ImplOutcome,
    impl_handoff_branch: str,
    base_ref: str,
    issue_url: str,
    cwd: str | None,
    session_dir: pathlib.Path,
) -> str:
    """Build the commit-pr prompt with diff context, run a fresh claude session,
    extract and return the PR URL from the event stream."""
    issue_num = issue_url.split("/")[-1] if issue_url else ""

    diff = _get_diff(impl_outcome, impl_handoff_branch, base_ref, cwd)

    if isinstance(impl_outcome, HeadAdvanced):
        run_kw: dict = dict(capture_output=True, text=True, check=False)
        if cwd:
            run_kw["cwd"] = cwd
        status_r = subprocess.run(["git", "status", "--porcelain"], **run_kw)
        worktree_dirty = bool(status_r.stdout.strip())
        if worktree_dirty:
            action_clause = _load("commit_pr_handoff_dirty.md").format(
                handoff_branch=impl_handoff_branch,
                commit_count=impl_outcome.commit_count,
                pre_head=base_ref,
            )
        else:
            action_clause = _load("commit_pr_handoff_clean.md").format(
                handoff_branch=impl_handoff_branch,
                commit_count=impl_outcome.commit_count,
                pre_head=base_ref,
            )
    else:
        # DirtyOnly: create branch + commit + push from scratch
        action_clause = _load("commit_pr_fresh.md")

    if issue_num:
        branch_clause = f"Name the branch 'issue-{issue_num}-<short-slug>'."
        closes_clause = (
            f"End the commit message with 'Closes #{issue_num}' and include "
            f"'Closes #{issue_num}' in the PR body."
        )
    else:
        branch_clause = "Name the branch with a short descriptive slug derived from the plan title."
        closes_clause = (
            "Do NOT include any 'Closes #N' or 'Fixes #N' link in the commit "
            "message or PR body."
        )

    prompt = (
        f"Here is the implementation diff:\n\n```diff\n{diff}\n```\n\n"
        f"{action_clause} {branch_clause} {closes_clause} "
        "Print ONLY the PR URL on the final line of your response."
    )

    completed: CompletedRun = client.run(
        prompt,
        label="commit-pr",
        model=model,
        raw_path=session_dir / "stream-commit-pr.jsonl",
        capture_events=True,
    )

    events = completed.events or []
    pr_url = extract_gh_url(
        events,
        url_pattern=r"https://github\.com/[^ )]+/pull/[0-9]+",
        cmd_pattern=r"gh pr create",
        label="PR",
    )
    return pr_url
