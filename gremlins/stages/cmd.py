"""Cmd stage — run a list of shell commands; return NeedsFix on non-zero exit."""

from __future__ import annotations

import asyncio
import pathlib
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, NeedsFix, Outcome


class Cmd(Stage):
    type = "cmd"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.n: int = 0

    async def run(self, state: State) -> Outcome:
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        if not cmds:
            return Done()
        self.n += 1
        combined = " && ".join(cmds)
        proc = await asyncio.create_subprocess_shell(
            combined,
            cwd=state.cwd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_b, stderr_b = await proc.communicate()
        output = stdout_b.decode() + stderr_b.decode()
        self._log_path(state).write_text(output, encoding="utf-8")
        if proc.returncode != 0:
            return NeedsFix(output, proc.returncode)
        return Done()

    def _log_path(self, state: State) -> pathlib.Path:
        raw = self.options.get("log_path")
        if raw:
            return state.session_dir / raw.format(n=self.n)
        return state.session_dir / "cmd.log"
