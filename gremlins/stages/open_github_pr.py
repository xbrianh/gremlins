"""Open-GitHub-PR stage for the gh pipeline."""

from __future__ import annotations

import logging
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.gh_utils import extract_gh_url
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import (
    append_artifact,
    last_artifact_branch,
    read_state_str,
    resolve_state_file,
)

logger = logging.getLogger(__name__)


def _load(name: str) -> str:
    return (BUNDLED_PROMPT_DIR / name).read_text(encoding="utf-8")


class OpenGitHubPR(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        issue_url: str,
        base_ref: str | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.issue_url = issue_url
        self.base_ref = base_ref

    def run(self, pipe: Any) -> str:
        sf = resolve_state_file(self.state.gr_id)
        base_ref = (
            last_artifact_branch(self.state.gr_id)
            or self.base_ref
            or read_state_str(sf, "base_ref_name")
            or "main"
        )

        issue_num = self.issue_url.split("/")[-1] if self.issue_url else ""

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
            label="open-github-pr",
            raw_path=self.state.session_dir / "stream-open-github-pr.jsonl",
            capture_events=True,
        )

        pr_url = extract_gh_url(
            completed.events or [],
            url_pattern=r"https://github\.com/[^ )]+/pull/[0-9]+",
            cmd_pattern=r"gh pr create",
            label="PR",
            text_result=completed.text_result,
        )
        impl_branch = read_state_str(sf, "impl_materialized_branch")
        append_artifact(self.state.gr_id, {"type": "pr", "url": pr_url, "branch": impl_branch})
        logger.info("PR: %s", pr_url)
        return pr_url


register_stage("open-github-pr", OpenGitHubPR)
