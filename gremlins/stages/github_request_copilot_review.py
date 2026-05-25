"""GitHub request-copilot-review stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils import proc


class GitHubRequestCopilotReview(Stage):
    type = "github-request-copilot-review"
    needs_gh = True

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_num: str = "",
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self._pr_num = pr_num

    async def run(self, state: State) -> Outcome:
        repo = state.repo
        pr_num = self._pr_num or str(state.artifacts.read("pr").number)
        r = await proc.run_async(
            [
                "gh",
                "pr",
                "edit",
                pr_num,
                "--repo",
                repo,
                "--add-reviewer",
                "copilot-pull-request-reviewer",
            ],
            cwd=state.cwd,
        )
        if r.returncode != 0:
            detail = r.stderr.strip() or r.stdout.strip()
            raise RuntimeError(
                f"could not request Copilot review (is it enabled in repo settings?): "
                f"exit {r.returncode}: {detail}"
            )
        return Done()
