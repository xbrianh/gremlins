"""Agent primitive stage: resolves in: artifacts, renders prompt, invokes agent, verifies out:."""

from __future__ import annotations

import json
from typing import Any, cast

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome


def _to_str(value: Any) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8")
    if isinstance(value, (dict, list)):
        return json.dumps(value, indent=2)
    return str(value)


class Agent(Stage):
    """YAML type: agent.

    in:  var_name -> registry_key   (resolved content substituted into prompt)
    out: registry_key -> uri_string (bound before run, verified after)
    """

    type = "agent"

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        in_map: dict[str, str] | None = None,
        out_map: dict[str, str] | None = None,
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.in_map = in_map or {}
        self.out_map = out_map or {}

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Agent:
        from gremlins.pipeline.loader import get_client_from_dict

        name = d.get("name") or ""
        raw_in: object = d.get("in") or {}
        raw_out: object = d.get("out") or {}
        if not isinstance(raw_in, dict):
            raise ValueError(f"stage {name!r}: 'in' must be a mapping")
        if not isinstance(raw_out, dict):
            raise ValueError(f"stage {name!r}: 'out' must be a mapping")
        stage = cls(
            name,
            d.get("prompt") or [],
            d.get("options") or {},
            in_map=dict(cast(dict[str, str], raw_in)),
            out_map=dict(cast(dict[str, str], raw_out)),
        )
        stage.client = get_client_from_dict(d)
        return stage

    async def run(self, state: State) -> Outcome:
        registry = state.artifacts
        if registry is None:
            registry = ArtifactRegistry(state.session_dir, cwd=state.worktree)

        for key, uri_str in self.out_map.items():
            registry.bind(key, Uri.parse(uri_str))

        subs: dict[str, str] = {}
        for var, key in self.in_map.items():
            subs[var] = _to_str(registry.read(key))

        template = "\n\n".join(self.prompts).rstrip()
        prompt = template.format(**subs) if subs else template

        raw_path = state.session_dir / f"stream-{self.name}.jsonl"
        await run_agent(state, prompt, label=self.name, raw_path=raw_path)

        for key, uri_str in self.out_map.items():
            uri = Uri.parse(uri_str)
            registry.resolver(uri.scheme).verify_produced(uri)

        return Done()
