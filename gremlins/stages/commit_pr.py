"""Commit-and-open-PR stage for the gh pipeline."""

from __future__ import annotations

import dataclasses
import pathlib
import subprocess

from ..clients.claude import CompletedRun
from ..gh_utils import extract_gh_url
from ..git import HeadAdvanced, ImplOutcome
from .context import StageContext

PROMPTS_DIR = pathlib.Path(__file__).resolve().parent.parent / "prompts"


@dataclasses.dataclass
class CommitPrOptions:
    model: str | None
    impl_outcome: ImplOutcome
    impl_handoff_branch: str
    base_ref: str
    issue_url: str
    cwd: str | None


def _load(name: str) -> str:
    return (PROMPTS_DIR / name).read_text(encoding="utf-8")


def _get_diff(
    outcome: ImplOutcome,
    impl_handoff_branch: str,
    base_ref: str,
    cwd: str | None,
) -> str:
    if isinstance(outcome, HeadAdvanced):
        r = subprocess.run(
            ["git", "log", "--patch", f"{base_ref}..{impl_handoff_branch}"],
            capture_output=True,
            text=True,
            check=False,
            cwd=cwd,
        )
        if r.returncode != 0:
            raise RuntimeError(
                f"git log --patch {base_ref}..{impl_handoff_branch} failed "
                f"(rc={r.returncode}): {r.stderr.strip()}"
            )
        diff = r.stdout.strip()
    else:
        r = subprocess.run(
            ["git", "diff", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
            cwd=cwd,
        )
        if r.returncode != 0:
            raise RuntimeError(
                f"git diff HEAD failed (rc={r.returncode}): {r.stderr.strip()}"
            )
        diff = r.stdout.strip()
    return diff or "(no diff available)"


def run(ctx: StageContext, options: CommitPrOptions) -> str:
    """Build the commit-pr prompt, run a fresh claude session, return the PR URL."""
    issue_num = options.issue_url.split("/")[-1] if options.issue_url else ""

    diff = _get_diff(options.impl_outcome, options.impl_handoff_branch, options.base_ref, options.cwd)

    if isinstance(options.impl_outcome, HeadAdvanced):
        status_r = subprocess.run(
            ["git", "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
            cwd=options.cwd,
        )
        worktree_dirty = bool(status_r.stdout.strip())
        if worktree_dirty:
            action_clause = _load("commit_pr_handoff_dirty.md").format(
                handoff_branch=options.impl_handoff_branch,
                commit_count=options.impl_outcome.commit_count,
                pre_head=options.base_ref,
            )
        else:
            action_clause = _load("commit_pr_handoff_clean.md").format(
                handoff_branch=options.impl_handoff_branch,
                commit_count=options.impl_outcome.commit_count,
                pre_head=options.base_ref,
            )
    else:
        action_clause = _load("commit_pr_fresh.md")

    if issue_num:
        branch_clause = f"Name the branch 'issue-{issue_num}-<short-slug>'."
        closes_clause = (
            f"End the commit message with 'Closes #{issue_num}' and include "
            f"'Closes #{issue_num}' in the PR body."
        )
    else:
        branch_clause = (
            "Name the branch with a short descriptive slug derived from the plan title."
        )
        closes_clause = (
            "Do NOT include any 'Closes #N' or 'Fixes #N' link in the commit "
            "message or PR body."
        )

    prompt = (
        f"Here is the implementation diff:\n\n```diff\n{diff}\n```\n\n"
        f"{action_clause} {branch_clause} {closes_clause} "
        "Print ONLY the PR URL on the final line of your response."
    )

    completed: CompletedRun = ctx.client.run(
        prompt,
        label="commit-pr",
        model=options.model,
        raw_path=ctx.session_dir / "stream-commit-pr.jsonl",
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
