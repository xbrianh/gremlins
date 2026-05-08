"""Open-GitHub-PR stage for the gh pipeline."""

from __future__ import annotations

import json
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.gh_utils import extract_gh_url
from gremlins.pipeline import StageEntry
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import patch_state, resolve_state_file


def _load(name: str) -> str:
    return (BUNDLED_PROMPT_DIR / name).read_text(encoding="utf-8")


class OpenGitHubPR(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        issue_url: str,
        base_ref: str | None = None,
    ) -> None:
        super().__init__(entry, model)
        self.issue_url = issue_url
        self.base_ref = base_ref

    def run(self, pipe: Any) -> str:
        sf = resolve_state_file(self.state.gr_id)
        base_ref = self.base_ref
        if base_ref is None and sf and sf.exists():
            try:
                base_ref = (
                    json.loads(sf.read_text(encoding="utf-8")).get("base_ref_name")
                    or ""
                )
            except Exception:
                base_ref = ""
        base_ref = base_ref or "main"

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
        patch_state(self.state.gr_id, pr_url=pr_url)
        return pr_url


register_stage("open-github-pr", OpenGitHubPR)
