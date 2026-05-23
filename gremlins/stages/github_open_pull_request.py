"""GitHub-Open-Pull-Request stage for the gh pipeline."""

from __future__ import annotations

import logging
import re
from typing import Any, cast

from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils import proc
from gremlins.utils.github import extract_gh_url
from gremlins.utils.yaml_io import render_bundled_prompt

logger = logging.getLogger(__name__)


def extract_pr_branch_from_events(events: list[dict[str, Any]]) -> str:
    """Extract the --head branch name from the last gh pr create Bash tool call."""
    for evt in reversed(events):
        if evt.get("type") != "assistant":
            continue
        msg = cast(dict[str, Any], evt.get("message") or {})
        for item in cast(list[Any], msg.get("content") or []):
            if item.get("type") != "tool_use" or item.get("name") != "Bash":
                continue
            inp = cast(dict[str, Any], item.get("input") or {})
            cmd = str(inp.get("command") or "")
            if "gh pr create" not in cmd:
                continue
            m = re.search(r"--head\s+(\S+)", cmd)
            if m:
                return m.group(1).strip("\"'")
    return ""


async def _get_pr_branch(pr_url: str) -> str:
    r = await proc.run_async(
        ["gh", "pr", "view", pr_url, "--json", "headRefName", "-q", ".headRefName"],
        timeout=15,
    )
    if r.returncode != 0:
        logger.warning("_get_pr_branch: gh pr view failed for %s", pr_url)
        return ""
    return r.stdout.strip()


class GitHubOpenPullRequest(Stage):
    type = "github-open-pull-request"
    needs_gh = True

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> GitHubOpenPullRequest:
        from gremlins.pipeline.loader import get_client_from_dict

        options: dict[str, Any] = d.get("options") or {}
        stage = cls(
            d["name"],
            d.get("prompt") or [],
            options,
            base_ref=options.get("base_ref") or None,
        )
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        base_ref: str | None = None,
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.base_ref = base_ref

    async def run(self, state: State) -> Outcome:
        issue_url = state.data.issue_url
        base_ref = (
            state.data.last_pr_branch()
            or self.base_ref
            or state.data.base_ref_name
            or "main"
        )

        issue_num = issue_url.split("/")[-1] if issue_url else ""

        if issue_num:
            closes_clause = f"Include 'Closes #{issue_num}' in the PR body."
        else:
            closes_clause = (
                "Do NOT include any 'Closes #N' or 'Fixes #N' link in the PR body."
            )

        base_prompt = render_bundled_prompt(
            "github_open_pull_request.md", base_ref=base_ref
        ).rstrip()

        n = state.data.loop_iteration
        iter_clause = (
            f" This is loop iteration {n}; append '-iter{n}' to the branch slug"
            f" to avoid colliding with a prior iteration's branch"
            f" (e.g. 'issue-NNN-some-slug-iter{n}')."
            if n > 1
            else ""
        )

        prompt = f"{base_prompt} {closes_clause}{iter_clause}"

        completed: CompletedRun = await run_agent(
            state,
            prompt,
            label="github-open-pull-request",
            raw_path=state.session_dir / "stream-github-open-pull-request.jsonl",
            capture_events=True,
        )

        events = completed.events or []
        pr_url = extract_gh_url(
            events,
            url_pattern=r"https://github\.com/[^ )]+/pull/[0-9]+",
            cmd_pattern=r"gh pr create",
            label="PR",
            text_result=completed.text_result,
        )
        branch = extract_pr_branch_from_events(events) or await _get_pr_branch(pr_url)
        if not branch:
            logger.warning("open-pr: could not determine PR branch for %s", pr_url)
        state.record_artifact({"type": "pr", "url": pr_url, "branch": branch})
        logger.info("PR: %s", pr_url)
        return Done()
