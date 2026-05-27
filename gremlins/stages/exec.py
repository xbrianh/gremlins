"""Exec primitive stage: runs shell commands with in:/out: artifact bindings."""

from __future__ import annotations

import os
import pathlib
import re
from typing import Any, cast

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.resolve import resolve_in_map
from gremlins.artifacts.schemes import FileSessionResolver, snapshot_head_before
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State
from gremlins.stages._passthrough import Passthrough as _Passthrough
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import proc as _proc

_CMD_SUB = re.compile(r"\{(\w+)\}")
_READ_SUB = re.compile(r"\{read:([-\w]+)\}")
_FRAMEWORK_KEYS = frozenset(["name", "model", "session_dir", "repo", "cwd", "base_ref"])
_STATUS_KEY = "status"


def _sub_reads(s: str, artifacts: ArtifactRegistry) -> str:
    def _r(m: re.Match[str]) -> str:
        key = m.group(1)
        raw = artifacts.read(key)
        if not isinstance(raw, bytes):
            raise TypeError(
                f"{{read:{key}}}: expected bytes artifact, got {type(raw).__name__}"
            )
        return raw.decode().strip()

    return _READ_SUB.sub(_r, s)


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
        for k in cast(dict[str, Any], d.get("options") or {}):
            if k in _FRAMEWORK_KEYS:
                raise ValueError(
                    f"stage {name!r}: option key {k!r} collides with framework substitution variable"
                )
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

        subs = dict(
            name=self.name,
            model=state.stage_model or state.client.model,
            session_dir=str(state.session_dir),
            repo=state.engine_ctx.repo,
            cwd=state.engine_ctx.cwd,
            base_ref=state.engine_ctx.base_ref,
        )
        for k, v in self.options.items():
            if k not in subs and isinstance(v, str):
                subs[k] = v
        _pt = _Passthrough(subs)

        pre_sha: str | None = None
        if any(v == "git://range" for v in self.out_map.values()):
            pre_sha = snapshot_head_before(cwd=pathlib.Path(state.engine_ctx.cwd))

        cmds = [
            _CMD_SUB.sub(lambda m: subs.get(m.group(1), m.group(0)), c.rstrip())
            for c in self.options.get("cmds", [])
            if c.strip()
        ]
        needs_fix = False
        if cmds:
            result = await _proc.run_shell_async(
                " && ".join(cmds),
                cwd=pathlib.Path(state.engine_ctx.cwd),
                env={**os.environ, **extra_env},
            )
            log_path = state.session_dir / f"exec-{self.name}.log"
            log_path.write_text(
                result.stdout + result.stderr or "(no output)\n", encoding="utf-8"
            )
            if result.returncode != 0:
                if _STATUS_KEY in self.out_map:
                    needs_fix = True
                else:
                    raise Bail(f"exec {self.name}: exited {result.returncode}")

        for raw_key, raw_uri_str in self.out_map.items():
            key = raw_key.format_map(_pt)
            uri_str = _sub_reads(raw_uri_str, state.artifacts).format_map(_pt)
            if uri_str == "git://range":
                if pre_sha is None:
                    raise RuntimeError(
                        f"exec {self.name}: git://range requires pre-snapshot"
                    )
                state.artifacts.bind_git_commit_range(key, pre_sha)
            else:
                uri = Uri.parse(uri_str)
                if key == _STATUS_KEY and uri.scheme == "file":
                    resolver = state.artifacts.resolver(uri.scheme)
                    if isinstance(resolver, FileSessionResolver):
                        resolver.write(uri, b"needs_fix\n" if needs_fix else b"pass\n")
                state.artifacts.bind(key, uri)
                state.artifacts.resolver(uri.scheme).verify_produced(uri)

        return Done()
