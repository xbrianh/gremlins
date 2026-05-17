"""GitHub request-copilot-review stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

import subprocess
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome


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

    def run(self, state: State) -> Outcome:
        repo = state.repo
        pr_num = self._pr_num or state.data.read_pr_num()
        if not pr_num:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        r = subprocess.run(
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
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode != 0:
            detail = r.stderr.strip() or r.stdout.strip()
            raise RuntimeError(
                f"could not request Copilot review (is it enabled in repo settings?): "
                f"exit {r.returncode}: {detail}"
            )
        return Done()
