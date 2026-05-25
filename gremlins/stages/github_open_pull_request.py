"""GitHub-Draft-Pull-Request stage: pushes branch and writes pr-draft.json."""

from __future__ import annotations

from typing import Any

from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils.yaml_io import render_bundled_prompt


class GitHubDraftPullRequest(Stage):
    type = "github-draft-pull-request"
    needs_gh = True

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> GitHubDraftPullRequest:
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
            state.artifacts.resolve("issue").path.removeprefix("issue/")
            if state.artifacts.produced("issue")
            else ""
        )

        if issue_num:
            closes_clause = f"Include 'Closes #{issue_num}' in the PR body."
        else:
            closes_clause = (
                "Do NOT include any 'Closes #N' or 'Fixes #N' link in the PR body."
            )

        base_prompt = render_bundled_prompt(
            "github_draft_pull_request.md",
            base_ref=base_ref,
            session_dir=str(state.session_dir),
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

        await run_agent(
            state,
            prompt,
            label="github-draft-pull-request",
            raw_path=state.session_dir / "stream-github-draft-pull-request.jsonl",
            capture_events=True,
        )
        return Done()
