"""Open-GitHub-PR stage for the gh pipeline."""

from __future__ import annotations

import logging
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.utils import proc
from gremlins.utils.github import extract_gh_url
from gremlins.utils.yaml import render_bundled_prompt

logger = logging.getLogger(__name__)


def _get_pr_branch(pr_url: str) -> str:
    r = proc.run(
        ["gh", "pr", "view", pr_url, "--json", "headRefName", "-q", ".headRefName"],
        timeout=15,
    )
    if r.returncode != 0:
        logger.warning("_get_pr_branch: gh pr view failed for %s", pr_url)
        return ""
    return r.stdout.strip()


class OpenGitHubPR(Stage):
    type = "open-github-pr"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> OpenGitHubPR:
        from gremlins.pipeline.loader import get_client_from_dict

        options: dict[str, Any] = d.get("options") or {}
        stage = cls(
            d["name"],
            None,
            d.get("prompt") or [],
            options,
            base_ref=options.get("base_ref") or None,
        )
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        base_ref: str | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.base_ref = base_ref

    def run(self, state: State) -> str:
        issue_url = state.issue_url
        base_ref = (
            state.last_pr_branch() or self.base_ref or state.base_ref_name or "main"
        )

        issue_num = issue_url.split("/")[-1] if issue_url else ""

        if issue_num:
            closes_clause = f"Include 'Closes #{issue_num}' in the PR body."
        else:
            closes_clause = (
                "Do NOT include any 'Closes #N' or 'Fixes #N' link in the PR body."
            )

        base_prompt = render_bundled_prompt(
            "open_github_pr.md", base_ref=base_ref
        ).rstrip()

        n = state.loop_iteration
        iter_clause = (
            f" This is loop iteration {n}; append '-iter{n}' to the branch slug"
            f" to avoid colliding with a prior iteration's branch"
            f" (e.g. 'issue-NNN-some-slug-iter{n}')."
            if n > 1
            else ""
        )

        prompt = f"{base_prompt} {closes_clause}{iter_clause}"

        completed: CompletedRun = self.run_claude(
            prompt,
            state=state,
            label="open-github-pr",
            raw_path=state.session_dir / "stream-open-github-pr.jsonl",
            capture_events=True,
        )

        pr_url = extract_gh_url(
            completed.events or [],
            url_pattern=r"https://github\.com/[^ )]+/pull/[0-9]+",
            cmd_pattern=r"gh pr create",
            label="PR",
            text_result=completed.text_result,
        )
        branch = _get_pr_branch(pr_url)
        state.append_artifact({"type": "pr", "url": pr_url, "branch": branch})
        logger.info("PR: %s", pr_url)
        return pr_url


register_stage("open-github-pr", OpenGitHubPR)
