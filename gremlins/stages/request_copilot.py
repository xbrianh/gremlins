"""Request-copilot stage: adds copilot-pull-request-reviewer to a PR."""

from __future__ import annotations

import dataclasses
import subprocess

from .context import StageContext


@dataclasses.dataclass
class RequestCopilotOptions:
    repo: str
    pr_num: str


def run(_ctx: StageContext, options: RequestCopilotOptions) -> None:
    r = subprocess.run(
        [
            "gh",
            "pr",
            "edit",
            options.pr_num,
            "--repo",
            options.repo,
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