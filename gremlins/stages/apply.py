from __future__ import annotations

import asyncio
from typing import Any

from gremlins.artifacts.schemes import snapshot_head_before
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
        if not cmds:
            return Done()

        pre_sha: str | None = None
        if state.artifacts is not None and not state.artifacts.produced("commits"):
            try:
                pre_sha = snapshot_head_before(cwd=state.cwd)
            except RuntimeError:
                pass  # not a git repo

        log_lines: list[str] = []
        for cmd in cmds:
            p = await asyncio.create_subprocess_shell(
                cmd,
                cwd=state.cwd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            out_b, err_b = await p.communicate()
            log_lines.append(out_b.decode() + err_b.decode())
            if p.returncode != 0:
                (state.session_dir / "apply.log").write_text(
                    "\n".join(log_lines), encoding="utf-8"
                )
                raise Bail(f"apply {self.name}: {cmd} exited {p.returncode}")
        self._maybe_commit(state)

        if state.artifacts is not None and pre_sha is not None:
            state.artifacts.bind_git_commit_range("commits", pre_sha)

        return Done()

    def _maybe_commit(self, state: State) -> None:
        if not proc.run_ok(["git", "rev-parse", "--git-dir"], cwd=state.cwd):
            return
        r = proc.run(["git", "add", "-A"], cwd=state.cwd)
        if r.returncode != 0:
            raise Bail(
                f"apply {self.name}: git add failed: {(r.stdout + r.stderr).strip()}"
            )
        if proc.run_ok(["git", "diff", "--cached", "--quiet"], cwd=state.cwd):
            return
        msg = self.options.get("commit_message") or self.name
        r = proc.run(["git", "commit", "-m", msg], cwd=state.cwd)
        if r.returncode != 0:
            raise Bail(
                f"apply {self.name}: git commit failed: {(r.stdout + r.stderr).strip()}"
            )
