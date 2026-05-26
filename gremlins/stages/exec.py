"""Exec primitive stage: runs shell commands with in:/out: artifact bindings."""

from __future__ import annotations

import os
import pathlib
from typing import Any, cast

from gremlins.artifacts.resolve import resolve_in_map
from gremlins.artifacts.schemes import GitHubResolver, snapshot_head_before
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State
from gremlins.stages._passthrough import Passthrough as _Passthrough
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils import proc as _proc


class Exec(Stage):
    type = "exec"

    def __init__(
        self,
        name: str,
        options: dict[str, Any],
        *,
        in_map: dict[str, str] | None = None,
        out_map: dict[str, str] | None = None,
    ) -> None:
        super().__init__(name)
        self.options = options
        self.in_map = in_map or {}
        self.out_map = out_map or {}

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Exec:
        name = d.get("name") or ""
        raw_in: object = d.get("in") or {}
        raw_out: object = d.get("out") or {}
        if not isinstance(raw_in, dict):
            raise ValueError(f"stage {name!r}: 'in' must be a mapping")
        if not isinstance(raw_out, dict):
            raise ValueError(f"stage {name!r}: 'out' must be a mapping")
        return cls(
            name,
            d.get("options") or {},
            in_map=dict(cast(dict[str, str], raw_in)),
            out_map=dict(cast(dict[str, str], raw_out)),
        )

    async def run(self, state: State) -> Outcome:
        try:
            extra_env = resolve_in_map(state.artifacts, self.in_map)
        except ValueError as exc:
            raise Bail(f"exec {self.name}: {exc}") from exc

        _pt = _Passthrough(
            dict(
                name=self.name,
                model=state.stage_model or state.client.model,
                session_dir=str(state.session_dir),
                repo=state.engine_ctx.repo,
                cwd=state.engine_ctx.cwd,
            )
        )

        pre_sha: str | None = None
        if any(v == "git://range" for v in self.out_map.values()):
            pre_sha = snapshot_head_before(cwd=pathlib.Path(state.engine_ctx.cwd))

        cmds = [
            c.rstrip().format_map(_pt)
            for c in self.options.get("cmds", [])
            if c.strip()
        ]
        stdout_str = ""
        stderr_str = ""
        if cmds:
            result = await _proc.run_shell_async(
                " && ".join(cmds),
                cwd=pathlib.Path(state.engine_ctx.cwd),
                env={**os.environ, **extra_env},
            )
            stdout_str = result.stdout
            stderr_str = result.stderr
            if result.returncode != 0:
                log_path = state.session_dir / f"exec-{self.name}.log"
                log_path.write_text(stdout_str + stderr_str, encoding="utf-8")
                if self.options.get("on_fail") == "needs_fix":
                    return NeedsFix(stdout_str + stderr_str, result.returncode)
                raise Bail(f"exec {self.name}: exited {result.returncode}")

        for raw_key, raw_uri_str in self.out_map.items():
            key = raw_key.format_map(_pt)
            uri_str = raw_uri_str.format_map(_pt)
            if uri_str == "git://range":
                if pre_sha is None:
                    raise RuntimeError(
                        f"exec {self.name}: git://range requires pre-snapshot"
                    )
                state.artifacts.bind_git_commit_range(key, pre_sha)
            elif uri_str == "gh://pr":
                resolver = cast(GitHubResolver, state.artifacts.resolver("gh"))
                try:
                    captured = resolver.capture(stdout_str, stderr_str)
                except ValueError as exc:
                    raise Bail(str(exc)) from exc
                state.artifacts.bind(key, captured)
            else:
                uri = Uri.parse(uri_str)
                state.artifacts.bind(key, uri)
                state.artifacts.resolver(uri.scheme).verify_produced(uri)

        return Done()
