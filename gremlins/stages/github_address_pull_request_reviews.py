"""GitHub address-pull-request-reviews stage."""

from __future__ import annotations

from typing import Any

from gremlins.executor.state import State
from gremlins.stages.agent import Agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Done, Outcome


class GitHubAddressPullRequestReviews(Stage):
    type = "github-address-pull-request-reviews"

    @classmethod
    def with_dict(
        cls, d: dict[str, Any], depth: int = 0
    ) -> GitHubAddressPullRequestReviews:
        prompts: list[str] = d.get("prompt") or []
        if not prompts:
            raise ValueError(
                f"stage {d['name']!r}: 'prompt' is required for github-address-pull-request-reviews"
            )
        stage = cls(d["name"], prompts, d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_url: str = "",
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.pr_url = pr_url

    async def run(self, state: State) -> Outcome:
        pr_url = self.pr_url or state.artifacts.read("pr").url
        prompt = (
            "\n\n".join(self.prompts)
            .rstrip()
            .format(
                pr_url=pr_url,
            )
        )
        agent = Agent(self.name, [prompt], self.options)
        await agent.run(state)
        return Done()
