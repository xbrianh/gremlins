"""Plan stage (local and GitHub)."""

from __future__ import annotations

import logging
import pathlib
import sys
from typing import Any

from gremlins.gh_utils import extract_gh_url, view_issue
from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import patch_state

logger = logging.getLogger(__name__)


def _fmt_escape(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


def _fetch_issue_body(issue_num: str, repo: str) -> str:
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


class Plan(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        instructions: str,
        plan_file: pathlib.Path | None = None,
        ref: str = "",
        repo: str = "",
    ) -> None:
        super().__init__(entry, model)
        self.instructions = instructions
        self.plan_file = plan_file
        self.ref = ref
        self.repo = repo

    def run(self, pipe: Any) -> None:
        target = getattr(pipe, "target", "local")
        getattr(self, f"results_to_{target}")(pipe)

    def results_to_local(self, pipe: Any) -> None:
        template = load_prompts(self.prompt_paths)
        prompt = template.format(
            plan_file=self.plan_file,
            instructions=self.instructions,
        )
        self.run_claude(
            prompt,
            label="plan",
            raw_path=self.state.session_dir / "stream-plan.jsonl",
        )
        if (
            self.plan_file is None
            or not self.plan_file.exists()
            or self.plan_file.stat().st_size == 0
        ):
            raise RuntimeError(f"plan stage did not produce {self.plan_file}")

    def results_to_github(self, pipe: Any) -> None:
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
        pipe.issue_url = issue_url
        pipe.issue_num = issue_num
        pipe.issue_body = issue_body


register_stage("plan", Plan)
