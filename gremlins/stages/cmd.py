"""Cmd stage — run a list of shell commands; return NeedsFix on non-zero exit."""

from __future__ import annotations

import pathlib
import subprocess
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, NeedsFix, Outcome


class Cmd(Stage):
    type = "cmd"

    def __init__(
        self, name: str, model: str | None, prompts: list[str], options: dict[str, Any]
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.n: int = 0

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Cmd:
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> Outcome:
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        if not cmds:
            return Done()
        self.n += 1
        combined = " && ".join(cmds)
        result = subprocess.run(
            combined,
            shell=True,
            cwd=state.cwd,
            capture_output=True,
            text=True,
        )
        output = result.stdout + result.stderr
        self._log_path(state).write_text(output, encoding="utf-8")
        if result.returncode != 0:
            return NeedsFix(output)
        return Done()

    def _log_path(self, state: State) -> pathlib.Path:
        raw = self.options.get("log_path")
        if raw:
            return state.session_dir / raw.format(n=self.n)
        return state.session_dir / "cmd.log"
