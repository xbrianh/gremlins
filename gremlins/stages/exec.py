"""Exec primitive stage: runs shell commands with in:/out: artifact bindings."""

from __future__ import annotations

import os
from typing import Any, cast

from gremlins.artifacts.schemes import GitHubResolver, snapshot_head_before
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils import proc
from gremlins.utils.text import to_str


class Exec(Stage):
    type = "exec"

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
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Exec:
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
        if (self.in_map or self.out_map) and state.artifacts is None:
            raise RuntimeError(
                f"exec {self.name}: in/out bindings require an artifact registry"
            )

        extra_env: dict[str, str] = {}
        if state.artifacts is not None:
            for var, key_path in self.in_map.items():
                key, _, field = key_path.partition(".")
                value = state.artifacts.read(key)
                if field and isinstance(value, dict):
                    extra_env[var] = str(cast(dict[str, Any], value).get(field, ""))
                else:
                    extra_env[var] = to_str(value)

        pre_sha: str | None = None
        if any(v == "git://range" for v in self.out_map.values()):
            pre_sha = snapshot_head_before(cwd=state.cwd)

        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        stdout_str = ""
        stderr_str = ""
        if cmds:
            result = await proc.run_shell_async(
                " && ".join(cmds),
                cwd=state.cwd,
                env={**os.environ, **extra_env},
            )
            stdout_str = result.stdout
            stderr_str = result.stderr
            if result.returncode != 0:
                all_output = stdout_str + stderr_str
                log_path = state.session_dir / f"exec-{self.name}.log"
                log_path.write_text(all_output, encoding="utf-8")
                on_fail = self.options.get("on_fail", "bail")
                if on_fail == "needs_fix":
                    return NeedsFix(all_output, result.returncode)
                raise Bail(f"exec {self.name}: exited {result.returncode}")

        if state.artifacts is not None:
            for key, uri_str in self.out_map.items():
                if uri_str == "git://range":
                    if pre_sha is None:
                        raise RuntimeError(
                            f"exec {self.name}: git://range requires a git repo"
                        )
                    state.artifacts.bind_git_commit_range(key, pre_sha)
                elif uri_str == "gh://pr":
                    resolver = cast(GitHubResolver, state.artifacts.resolver("gh"))
                    captured_uri = resolver.capture(stdout_str, stderr_str)
                    state.artifacts.bind(key, captured_uri)
                else:
                    uri = Uri.parse(uri_str)
                    state.artifacts.bind(key, uri)
                    state.artifacts.resolver(uri.scheme).verify_produced(uri)

        return Done()
