"""ghreview stage for the gh pipeline."""

from __future__ import annotations

import pathlib

from ..clients.claude import ClaudeClient
from ..state import check_bail


def run_ghreview_stage(
    *,
    client: ClaudeClient,
    model: str | None,
    pr_url: str,
    artifacts_dir: pathlib.Path,
    code_style: str,
) -> None:
    """Run /ghreview. Calls check_bail after completion."""
    prompt = f"## Coding style\n\n{code_style}\n\n/ghreview {pr_url}"
    client.run(
        prompt,
        label="ghreview",
        model=model,
        raw_path=artifacts_dir / "stream-ghreview.jsonl",
    )
    check_bail("/ghreview")
