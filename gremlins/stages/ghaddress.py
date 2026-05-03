"""ghaddress stage for the gh pipeline."""

from __future__ import annotations

import dataclasses

from ..state import check_bail
from .context import StageContext


@dataclasses.dataclass
class GhaddressOptions:
    model: str | None
    pr_url: str
    code_style: str


def run(ctx: StageContext, options: GhaddressOptions) -> None:
    """Run /ghaddress on the PR. Calls check_bail after completion."""
    prompt = f"## Coding style\n\n{options.code_style}\n\n/ghaddress {options.pr_url}"
    ctx.client.run(
        prompt,
        label="ghaddress",
        model=options.model,
        raw_path=ctx.session_dir / "stream-ghaddress.jsonl",
    )
    check_bail("/ghaddress")
