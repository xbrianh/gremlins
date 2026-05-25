"""Push local commits to the PR branch recorded in state.artifacts."""

from __future__ import annotations

import asyncio
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome


class GitHubPushToPrBranch(Stage):
    type = "github-push-to-pr-branch"

    def __init__(
        self, name: str, _prompts: list[str], _options: dict[str, Any]
    ) -> None:
        super().__init__(name)

    async def run(self, state: State) -> Outcome:
        if not state.artifacts.produced("pr"):
            raise Bail("no PR recorded in registry — launch with --pr <num|url>")
        branch = state.artifacts.read("pr").branch
        proc = await asyncio.create_subprocess_exec(
            "git",
            "push",
            "origin",
            f"HEAD:{branch}",
            cwd=state.cwd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_b, stderr_b = await proc.communicate()
        if proc.returncode != 0:
            output = (stdout_b + stderr_b).decode(errors="replace").strip()
            raise Bail(f"git push origin HEAD:{branch} failed: {output}")
        return Done()
