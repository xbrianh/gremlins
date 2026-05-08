"""Request-copilot stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

import json
from typing import Any

from gremlins.pipeline import StageEntry
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import resolve_state_file


def _read_pr_num(gr_id: str | None) -> str:
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return ""
    try:
        pr_url = json.loads(sf.read_text(encoding="utf-8")).get("pr_url") or ""
        return pr_url.split("/")[-1] if pr_url else ""
    except (json.JSONDecodeError, OSError):
        return ""


class RequestCopilot(Stage):
    def __init__(
        self, entry: StageEntry, model: str | None, *, repo: str, pr_num: str = ""
    ) -> None:
        super().__init__(entry, model)
        self._repo = repo
        self._pr_num = pr_num

    def run(self, pipe: Any) -> None:
        repo = self._repo
        pr_num = self._pr_num or _read_pr_num(self.state.gr_id)
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
