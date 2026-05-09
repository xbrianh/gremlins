"""Request-copilot stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

from typing import Any

from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import read_pr_num


class RequestCopilot(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        repo: str,
        pr_num: str = "",
    ) -> None:
        super().__init__(name, model, prompts, options)
        self._repo = repo
        self._pr_num = pr_num

    def run(self, pipe: Any) -> None:
        repo = self._repo
        pr_num = self._pr_num or read_pr_num(self.state.gr_id)
        if not pr_num:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        r = self.run_subprocess(
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


register_stage("request-copilot", RequestCopilot)
