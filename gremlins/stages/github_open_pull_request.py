"""GitHub-Open-Pull-Request stage for the gh pipeline."""

from __future__ import annotations

import logging
import re
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils.github import extract_gh_url

logger = logging.getLogger(__name__)

_PROMPT = (
    "You are creating a GitHub pull request for changes on a detached HEAD.\n\n"
    "Push the current HEAD to a new branch and create a pull request targeting"
    " `{base_ref}` using `gh pr create`."
    " Choose a descriptive branch name, write a clear title and body.{closes}{iter}"
)


class GitHubOpenPullRequest(Stage):
    type = "github-open-pull-request"
    needs_gh = True

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> GitHubOpenPullRequest:
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
        prev_branch = (
            state.artifacts.read("pr").branch if state.artifacts.produced("pr") else ""
        )
        _default_base = (
            state.artifacts.resolve("base_ref").path.removeprefix("ref/")
            if state.artifacts.produced("base_ref")
            else "main"
        )
        base_ref = prev_branch or self.base_ref or _default_base
        issue_num = (
            state.artifacts.resolve("plan").path.removeprefix("issue/")
            if state.artifacts.produced("plan")
            and state.artifacts.resolve("plan").scheme == "gh"
            else ""
        )

        if issue_num:
            closes_clause = f" Include 'Closes #{issue_num}' in the PR body."
        else:
            closes_clause = (
                " Do NOT include any 'Closes #N' or 'Fixes #N' link in the PR body."
            )

        n = state.data.loop_iteration
        iter_clause = (
            f" This is loop iteration {n}; append '-iter{n}' to the branch slug"
            f" (e.g. 'issue-NNN-some-slug-iter{n}')."
            if n > 1
            else ""
        )

        prompt = _PROMPT.format(
            base_ref=base_ref,
            closes=closes_clause,
            iter=iter_clause,
        )

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
        _m = re.search(r"/pull/(\d+)", pr_url)
        if not _m:
            raise RuntimeError(f"could not parse PR number from {pr_url!r}")
        from gremlins.artifacts.uri import Uri  # noqa: PLC0415

        state.artifacts.bind("pr", Uri.parse(f"gh://pr/{_m.group(1)}"), override=True)
        logger.info("PR: %s", pr_url)
        return Done()
