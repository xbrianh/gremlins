"""GitHub request-copilot-review stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

from typing import Any, cast

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

    def _read_pr_num(self, state: State) -> str:
        if state.artifacts is not None:
            try:
                pr_data = state.artifacts.read("pr")
                if isinstance(pr_data, dict):
                    return str(cast(dict[str, Any], pr_data).get("number") or "")
            except Exception:
                pass
        return state.data.read_pr_num()

    async def run(self, state: State) -> Outcome:
        repo = state.repo
        pr_num = self._pr_num or self._read_pr_num(state)
        if not pr_num:
            raise RuntimeError("no 'pr' artifact bound (rewind to open-pr?)")
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
