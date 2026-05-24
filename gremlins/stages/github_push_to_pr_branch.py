"""Push local commits to the PR branch recorded in state.artifacts."""

from __future__ import annotations

import asyncio
from typing import Any, cast

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome


class GitHubPushToPrBranch(Stage):
    type = "github-push-to-pr-branch"

    def __init__(
        self, name: str, _prompts: list[str], _options: dict[str, Any]
    ) -> None:
        super().__init__(name)

    def _resolve_branch(self, state: State) -> str:
        if state.artifacts is not None:
            try:
                pr_data = state.artifacts.read("pr")
                if isinstance(pr_data, dict):
                    return str(cast(dict[str, Any], pr_data).get("branch") or "")
            except Exception:
                pass
        return state.data.last_pr_branch()

    async def run(self, state: State) -> Outcome:
        branch = self._resolve_branch(state)
        if not branch:
            raise Bail("no 'pr' artifact bound — rewind to open-pr?")
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
