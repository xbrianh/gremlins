from __future__ import annotations

import asyncio
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.utils import proc


class Apply(Stage):
    type = "apply"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options

    async def run(self, state: State) -> Outcome:
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        for cmd in cmds:
            p = await asyncio.create_subprocess_shell(
                cmd, cwd=state.cwd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
            )
            out_b, err_b = await p.communicate()
            if p.returncode != 0:
                (state.session_dir / "apply.log").write_text(
                    out_b.decode() + err_b.decode(), encoding="utf-8"
                )
                raise Bail(f"apply {self.name}: {cmd} exited {p.returncode}")
        self._maybe_commit(state)
        return Done()

    def _maybe_commit(self, state: State) -> None:
        proc.run(["git", "add", "-A"], cwd=state.cwd)
        if proc.run_ok(["git", "diff", "--cached", "--quiet"], cwd=state.cwd):
            return
        msg = self.options.get("commit_message") or self.name
        proc.run(["git", "commit", "-m", msg], cwd=state.cwd, check=True)
