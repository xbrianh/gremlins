"""GH plan stage — posts a GitHub issue and returns its URL/body."""

from __future__ import annotations

import dataclasses
import logging
import sys
from typing import Any

from gremlins.gh_utils import extract_gh_url, view_issue
from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.state import patch_state

logger = logging.getLogger(__name__)


def _fmt_escape(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


def _fetch_issue_body(issue_num: str, repo: str) -> str:
    """Fetch issue body from GitHub; raises SystemExit on failure."""
    try:
        issue_data = view_issue(issue_num, repo)
    except RuntimeError as exc:
        sys.stderr.write(f"error: could not fetch issue #{issue_num} body: {exc}\n")
        sys.stderr.flush()
        sys.exit(1)
    body = (issue_data.get("body") or "").strip()
    if not body:
        sys.stderr.write(f"error: issue #{issue_num} has an empty body\n")
        sys.stderr.flush()
        sys.exit(1)
    return body


@dataclasses.dataclass
class GHPlanResult:
    issue_url: str
    issue_num: str
    issue_body: str


class GHPlan(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        ref: str,
        instructions: str,
        repo: str,
    ) -> None:
        super().__init__(entry, model)
        self.ref = ref
        self.instructions = instructions
        self.repo = repo

    def run(self, pipe: Any) -> GHPlanResult:
        plan_prompt = load_prompts(self.prompt_paths).format(
            ref=_fmt_escape(self.ref or ""),
            instructions=_fmt_escape(self.instructions),
        )
        completed = self.run_claude(
            plan_prompt,
            label="plan",
            raw_path=self.state.session_dir / "ghplan-out.jsonl",
            capture_events=True,
        )
        issue_url = extract_gh_url(
            completed.events or [],
            url_pattern=r"https://github\.com/[^ )]+/issues/[0-9]+",
            cmd_pattern=r"gh issue create",
            label="issue",
            text_result=completed.text_result,
        )
        issue_num = issue_url.split("/")[-1]
        logger.info("issue: %s", issue_url)
        patch_state(self.state.gr_id, issue_url=issue_url, issue_num=issue_num)
        issue_body = _fetch_issue_body(issue_num, self.repo)
        return GHPlanResult(
            issue_url=issue_url, issue_num=issue_num, issue_body=issue_body
        )
