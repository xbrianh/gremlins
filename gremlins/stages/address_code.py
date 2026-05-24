"""Address-code stage."""

from __future__ import annotations

from typing import Any, cast

from gremlins.artifacts.registry import MissingArtifact
from gremlins.executor.state import State
from gremlins.stages.agent import Agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Done, Outcome


class AddressCode(Stage):
    type = "address-code"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> AddressCode:
        name = d.get("name") or ""
        raw_in: object = d.get("in") or {}
        if not isinstance(raw_in, dict):
            raise ValueError(f"stage {name!r}: 'in' must be a mapping")
        stage = cls(
            name,
            d.get("prompt") or [],
            d.get("options") or {},
            in_map=dict(cast(dict[str, str], raw_in)),
        )
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        in_map: dict[str, str] | None = None,
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.in_map = in_map or {}

    async def run(self, state: State) -> Outcome:
        agent = Agent(self.name, self.prompts, self.options, in_map=self.in_map)
        await agent.run(state)
        return Done()


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
