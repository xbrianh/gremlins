"""Agent primitive stage: resolves in: artifacts, renders prompt, invokes agent, verifies out:."""

from __future__ import annotations

from typing import Any, cast

from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils.text import to_str


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
        if state.artifacts is None:
            raise RuntimeError(f"stage {self.name!r}: state.artifacts is None")

        subs: dict[str, str] = {}
        if self.in_map or self.out_map:
            for key, uri_str in self.out_map.items():
                state.artifacts.bind(key, Uri.parse(uri_str))
            for var, key in self.in_map.items():
                subs[var] = to_str(state.artifacts.read(key))

        template = "\n\n".join(self.prompts).rstrip()
        prompt = template.format(**subs) if subs else template

        raw_path = state.session_dir / f"stream-{self.name}.jsonl"
        opts = dict(self.options)
        model = cast(str | None, opts.pop("model", None))
        await run_agent(
            state, prompt, label=self.name, raw_path=raw_path, model=model, **opts
        )

        for key, uri_str in self.out_map.items():
            uri = Uri.parse(uri_str)
            state.artifacts.resolver(uri.scheme).verify_produced(uri)

        return Done()
