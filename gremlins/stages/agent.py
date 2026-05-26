"""Agent primitive stage: resolves in: artifacts, renders prompt, invokes agent, verifies out:."""

from __future__ import annotations

from typing import Any, cast

from gremlins.artifacts.resolve import resolve_in_map
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State
from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Bail, Done, Outcome


class _Passthrough(dict):
    """format_map helper: unknown {key} passes through unchanged."""

    def __missing__(self, key: str) -> str:
        return "{" + key + "}"


class Agent(Stage):
    """YAML type: agent.

    in:  var_name -> registry_key   (resolved content substituted into prompt)
    out: registry_key -> uri_string (bound before run, verified after)

    Per-stage substitution vars available in prompts and out: URIs:
      {name}        — this stage's name
      {model}       — effective model (state.stage_model or state.client.model)
      {session_dir} — absolute path to the session directory
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
        opts = dict(self.options)
        raw_model = cast(str | None, opts.pop("model", None))

        try:
            subs = resolve_in_map(state.artifacts, self.in_map)
        except ValueError as exc:
            raise Bail(f"agent {self.name}: {exc}") from exc
        subs.update(
            name=self.name,
            model=state.stage_model or state.client.model,
            session_dir=str(state.session_dir),
        )

        out_map = {
            k.format_map(_Passthrough(subs)): v.format_map(_Passthrough(subs))
            for k, v in self.out_map.items()
        }
        for key, uri_str in out_map.items():
            state.artifacts.bind(key, Uri.parse(uri_str))

        template = "\n\n".join(self.prompts).rstrip()
        prompt = template.format_map(_Passthrough(subs))

        raw_path = state.session_dir / f"stream-{self.name}.jsonl"
        model = raw_model.format_map(_Passthrough(subs)) if raw_model else None
        await run_agent(
            state, prompt, label=self.name, raw_path=raw_path, model=model, **opts
        )

        for key, uri_str in out_map.items():
            uri = Uri.parse(uri_str)
            state.artifacts.resolver(uri.scheme).verify_produced(uri)

        return Done()
