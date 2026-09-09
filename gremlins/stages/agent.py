"""Agent primitive stage: resolves interpolation artifacts, renders prompt, invokes agent, verifies bind outputs."""

from __future__ import annotations

import logging
import pathlib
from typing import TYPE_CHECKING, Any, cast

from _gremlins_core.artifacts import Uri, resolve_interpolation_map
from _gremlins_core.stages import FRAMEWORK_KEYS, Bail, Done, Outcome

from gremlins.stages.agent_runner import run_agent
from gremlins.stages.base import Stage, get_client_from_dict

logger = logging.getLogger(__name__)

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


class Agent(Stage):
    type = "agent"

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        interpolation_map: dict[str, str] | None = None,
        bind_map: dict[str, str] | None = None,
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.interpolation_map = interpolation_map or {}
        self.bind_map = bind_map or {}

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Agent:
        name = d.get("name") or ""
        raw_interpolation: object = d.get("interpolation") or {}
        raw_bind: object = d.get("bind") or {}
        if "in" in d or "out" in d:
            raise ValueError(
                f"stage {name!r}: 'in'/'out' keys are no longer supported; "
                f"use 'interpolation'/'bind'"
            )
        if not isinstance(raw_interpolation, dict):
            raise ValueError(f"stage {name!r}: 'interpolation' must be a mapping")
        if not isinstance(raw_bind, dict):
            raise ValueError(f"stage {name!r}: 'bind' must be a mapping")
        for k in cast(dict[str, Any], d.get("options") or {}):
            if k in FRAMEWORK_KEYS - {"model"}:
                raise ValueError(
                    f"stage {name!r}: option key {k!r} collides with framework substitution variable"
                )
        stage = cls(
            name,
            d.get("prompt") or [],
            d.get("options") or {},
            interpolation_map=cast(dict[str, str], raw_interpolation),
            bind_map=cast(dict[str, str], raw_bind),
        )
        client = get_client_from_dict(d)
        stage.client = client
        stage.client_explicit = client is not None
        return stage

    async def run(self, gremlin: Gremlin) -> Outcome:
        state = gremlin.state
        if state is None:
            raise RuntimeError("agent stage requires gremlin.state to be initialized")
        opts = dict(self.options)
        raw_model = cast(str | None, opts.pop("model", None))

        try:
            counter = state.loop_iter
            interpolation_map = resolve_interpolation_map(
                state.artifacts, self.interpolation_map, loop_iter=counter
            )
        except ValueError as exc:
            raise Bail(f"agent {self.name}: {exc}") from exc

        # Register bind URIs and collect output paths
        bind_paths: dict[str, str] = {}
        # Track which bind keys are optional (end with ?)
        optional_keys: set[str] = set()
        for raw_key, raw_uri_str in self.bind_map.items():
            key = self.substitute_vars(raw_key, state, interpolation_map)
            optional = key.endswith("?")
            if optional:
                key = key[:-1]
                optional_keys.add(key)
            uri_str = self.substitute_vars(raw_uri_str, state, interpolation_map)
            uri_str = uri_str.replace("{loop_iter}", counter)
            uri = Uri.parse(uri_str)
            bind_paths[key] = state.artifacts.register(uri)

        # Merge: bind output paths shadow interpolation keys on collision
        subst_vars = {**interpolation_map, **bind_paths}

        template = "\n\n".join(self.prompts).rstrip()
        prompt = self.substitute_vars(template, state, subst_vars)

        # Inject workspace preamble
        workspace_parts: list[str] = []
        if state.cwd:
            workspace_parts.append(f"Your working directory is: {state.cwd}")
        wt = str(state.worktree) if state.worktree else ""
        if wt and wt != state.cwd:
            workspace_parts.append(f"Project worktree: {wt}")
        workspace_parts.append(
            "Relevant environment variables: $GREMLINS_WORKTREE_PATH, $GREMLIN_WORKSPACE_DIR, $GREMLINS_ARTIFACT_DIR"
        )
        if workspace_parts:
            preamble = "\n".join(workspace_parts)
            prompt = preamble + "\n\n" + prompt

        raw_path = state.artifact_dir / f"stream-{self.name}.jsonl"
        model = (
            self.substitute_vars(raw_model, state, subst_vars) if raw_model else None
        )

        # Single-output stages: verify artifact was produced
        single = len(bind_paths) == 1

        await run_agent(
            state,
            prompt,
            label=self.name,
            raw_path=raw_path,
            model=model,
            expected_artifact_paths=list(bind_paths.values()),
            artifact_reminder_count=3,
            **opts,
        )

        for key, uri_str in bind_paths.items():
            optional = key in optional_keys
            if not single:
                # Multi-output stages are best-effort
                continue
            p = pathlib.Path(uri_str)
            if not p.exists() or p.stat().st_size == 0:
                raise Bail(f"agent {self.name}: artifact {key} was not produced")

        return Done()
