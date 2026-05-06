"""Commit stage for the gh pipeline."""

from __future__ import annotations

from typing import Any

from gremlins.git import (
    HeadAdvanced,
    ImplOutcome,
    diff_output,
    has_dirty_worktree,
    log_patch,
)
from gremlins.pipeline import StageEntry
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage


def _load(name: str) -> str:
    return (BUNDLED_PROMPT_DIR / name).read_text(encoding="utf-8")


def _get_diff(
    outcome: ImplOutcome,
    impl_handoff_branch: str,
    base_ref: str,
    cwd: str | None,
) -> str:
    if isinstance(outcome, HeadAdvanced):
        diff = log_patch(f"{base_ref}..{impl_handoff_branch}", cwd=cwd).strip()
    else:
        diff = diff_output(["HEAD"], cwd=cwd).strip()
    return diff or "(no diff available)"


class Commit(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        impl_outcome: ImplOutcome,
        impl_handoff_branch: str,
        base_ref: str,
        issue_url: str,
        cwd: str | None = None,
    ) -> None:
        super().__init__(entry, model)
        self.impl_outcome = impl_outcome
        self.impl_handoff_branch = impl_handoff_branch
        self.base_ref = base_ref
        self.issue_url = issue_url
        self._cwd = cwd

    def run(self, pipe: Any) -> None:
        issue_num = self.issue_url.split("/")[-1] if self.issue_url else ""
        cwd_arg = self._cwd or (
            str(self.state.worktree) if self.state.worktree is not None else None
        )

        diff = _get_diff(
            self.impl_outcome, self.impl_handoff_branch, self.base_ref, cwd_arg
        )

        if isinstance(self.impl_outcome, HeadAdvanced):
            worktree_dirty = has_dirty_worktree(cwd=cwd_arg)
            if worktree_dirty:
                action_clause = _load("commit_handoff_dirty.md").format(
                    handoff_branch=self.impl_handoff_branch,
                    commit_count=self.impl_outcome.commit_count,
                    pre_head=self.base_ref,
                )
            else:
                action_clause = _load("commit_handoff_clean.md").format(
                    handoff_branch=self.impl_handoff_branch,
                    commit_count=self.impl_outcome.commit_count,
                    pre_head=self.base_ref,
                )
        else:
            action_clause = _load("commit_fresh.md")

        if issue_num:
            branch_clause = f"Name the branch 'issue-{issue_num}-<short-slug>'."
            closes_clause = f"End the commit message with 'Closes #{issue_num}'."
        else:
            branch_clause = "Name the branch with a short descriptive slug derived from the plan title."
            closes_clause = "Do NOT include any 'Closes #N' or 'Fixes #N' link in the commit message."

        prompt = (
            f"Here is the implementation diff:\n\n```diff\n{diff}\n```\n\n"
            f"{action_clause} {branch_clause} {closes_clause}"
        )

        self.run_claude(
            prompt,
            label="commit",
            raw_path=self.state.session_dir / "stream-commit.jsonl",
            capture_events=True,
        )


register_stage("commit", Commit)
