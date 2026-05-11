"""Request-copilot stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage


class RequestCopilot(Stage):
    type = "request-copilot"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> RequestCopilot:
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_num: str = "",
    ) -> None:
        super().__init__(name, model, prompts, options)
        self._pr_num = pr_num

    def run(self, state: State) -> None:
        repo = state.repo
        pr_num = self._pr_num or state.read_pr_num()
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
            state,
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
