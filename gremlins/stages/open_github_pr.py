"""Open-GitHub-PR stage for the gh pipeline."""

from __future__ import annotations

import logging
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.gh_utils import extract_gh_url
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.base import RuntimeState, Stage
from gremlins.stages.registry import register_stage
from gremlins.state import append_artifact, last_pr_branch

logger = logging.getLogger(__name__)


def _load(name: str) -> str:
    return (BUNDLED_PROMPT_DIR / name).read_text(encoding="utf-8")


class OpenGitHubPR(Stage):
    type = "open-github-pr"

    @classmethod
    def from_yaml(cls, d: dict[str, Any], depth: int = 0) -> OpenGitHubPR:
        from gremlins.pipeline.loader import get_client_from_yaml

        options: dict[str, Any] = d.get("options") or {}
        stage = cls(
            d["name"],
            None,
            d.get("prompt") or [],
            options,
            base_ref=options.get("base_ref") or None,
        )
        stage.client = get_client_from_yaml(d)
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

    def run(self, state: RuntimeState) -> str:
        issue_url = state.issue_url
        base_ref = (
            last_pr_branch(state.gr_id)
            or self.base_ref
            or state.base_ref_name
            or "main"
        )

        issue_num = issue_url.split("/")[-1] if issue_url else ""

        if issue_num:
            closes_clause = f"Include 'Closes #{issue_num}' in the PR body."
        else:
            closes_clause = (
                "Do NOT include any 'Closes #N' or 'Fixes #N' link in the PR body."
            )

        base_prompt = _load("open_github_pr.md").format(base_ref=base_ref).rstrip()
        prompt = f"{base_prompt} {closes_clause}"

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
        impl_branch = state.impl_materialized_branch
        if not impl_branch:
            raise RuntimeError(
                "impl_materialized_branch is empty; materialize-to-branch must run before open-github-pr"
            )
        append_artifact(
            state.gr_id, {"type": "pr", "url": pr_url, "branch": impl_branch}
        )
        logger.info("PR: %s", pr_url)
        return pr_url


register_stage("open-github-pr", OpenGitHubPR)
