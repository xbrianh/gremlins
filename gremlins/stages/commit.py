"""Commit stage for the gh pipeline."""

from __future__ import annotations

import json
import pathlib
from typing import Any

from gremlins.git import (
    DirtyOnly,
    GitError,
    HeadAdvanced,
    ImplOutcome,
    diff_output,
    has_dirty_worktree,
    log_patch,
    rev_list_count,
)
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import resolve_state_file


def _load(name: str) -> str:
    return (BUNDLED_PROMPT_DIR / name).read_text(encoding="utf-8")


def _read_state(sf: pathlib.Path | None, field: str) -> str:
    if sf is None or not sf.exists():
        return ""
    try:
        return json.loads(sf.read_text(encoding="utf-8")).get(field) or ""
    except (json.JSONDecodeError, OSError):
        return ""


def _get_diff(
    outcome: ImplOutcome,
    impl_materialized_branch: str,
    base_ref: str,
    cwd: str | None,
) -> str:
    if isinstance(outcome, HeadAdvanced):
        diff = log_patch(f"{base_ref}..{impl_materialized_branch}", cwd=cwd).strip()
    else:
        diff = diff_output(["HEAD"], cwd=cwd).strip()
    return diff or "(no diff available)"


class Commit(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        impl_outcome: ImplOutcome | None = None,
        impl_materialized_branch: str | None = None,
        base_ref: str | None = None,
        issue_url: str | None = None,
        cwd: str | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.impl_outcome = impl_outcome
        self.impl_materialized_branch = impl_materialized_branch
        self.base_ref = base_ref
        self.issue_url = issue_url
        self._cwd = cwd

    def _resolve_inputs(
        self,
    ) -> tuple[ImplOutcome, str, str, str]:
        sf = resolve_state_file(self.state.gr_id)

        impl_materialized_branch = self.impl_materialized_branch or _read_state(
            sf, "impl_materialized_branch"
        )
        base_ref = self.base_ref or _read_state(sf, "impl_base_ref")
        if not base_ref:
            raise RuntimeError("no impl_base_ref in state.json (rewind to implement?)")

        issue_url = self.issue_url or _read_state(sf, "issue_url")

        if impl_materialized_branch:
            try:
                commit_count = rev_list_count(f"{base_ref}..{impl_materialized_branch}")
            except GitError as exc:
                raise RuntimeError(str(exc)) from exc
            impl_outcome: ImplOutcome = HeadAdvanced(commit_count=commit_count)
        else:
            impl_outcome = DirtyOnly()

        return (impl_outcome, impl_materialized_branch, base_ref, issue_url)

    def run(self, pipe: Any) -> None:
        if self.impl_outcome is None:
            impl_outcome, impl_materialized_branch, base_ref, issue_url = (
                self._resolve_inputs()
            )
        else:
            impl_outcome = self.impl_outcome
            impl_materialized_branch = self.impl_materialized_branch or ""
            base_ref = self.base_ref or ""
            issue_url = self.issue_url or ""

        issue_num = issue_url.split("/")[-1] if issue_url else ""
        cwd_arg = self._cwd or (
            str(self.state.worktree) if self.state.worktree is not None else None
        )

        diff = _get_diff(impl_outcome, impl_materialized_branch, base_ref, cwd_arg)

        if isinstance(impl_outcome, HeadAdvanced):
            worktree_dirty = has_dirty_worktree(cwd=cwd_arg)
            if worktree_dirty:
                action_clause = _load("commit_handoff_dirty.md").format(
                    handoff_branch=impl_materialized_branch,
                    commit_count=impl_outcome.commit_count,
                    pre_head=base_ref,
                )
            else:
                action_clause = _load("commit_handoff_clean.md").format(
                    handoff_branch=impl_materialized_branch,
                    commit_count=impl_outcome.commit_count,
                    pre_head=base_ref,
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
