"""ghaddress stage for the gh pipeline."""

from __future__ import annotations

import pathlib

from ..clients.claude import ClaudeClient
from ..state import check_bail


def run_ghaddress_stage(
    *,
    client: ClaudeClient,
    model: str | None,
    pr_url: str,
    artifacts_dir: pathlib.Path,
    code_style: str,
) -> None:
    """Run /ghaddress on the PR. Calls check_bail after completion."""
    prompt = f"## Coding style\n\n{code_style}\n\n/ghaddress {pr_url}"
    client.run(
        prompt,
        label="ghaddress",
        model=model,
        raw_path=artifacts_dir / "stream-ghaddress.jsonl",
    )
    check_bail("/ghaddress")
