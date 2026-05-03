"""ghreview stage for the gh pipeline."""

from __future__ import annotations

import dataclasses

from ..state import check_bail
from .context import StageContext
from .registry import register_stage


@dataclasses.dataclass
class GhreviewOptions:
    model: str | None
    pr_url: str
    code_style: str


def run(ctx: StageContext, options: GhreviewOptions) -> None:
    """Run /ghreview. Calls check_bail after completion."""
    prompt = f"## Coding style\n\n{options.code_style}\n\n/ghreview {options.pr_url}"
    ctx.client.run(
        prompt,
        label="ghreview",
        model=options.model,
        raw_path=ctx.session_dir / "stream-ghreview.jsonl",
    )
    check_bail("/ghreview")


register_stage("ghreview", run)
