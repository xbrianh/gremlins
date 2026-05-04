"""ghreview stage for the gh pipeline."""

from __future__ import annotations

import dataclasses
import pathlib

from ..prompts import load_prompts
from ..state import check_bail
from .context import StageContext
from .registry import register_stage


@dataclasses.dataclass
class GhreviewOptions:
    model: str | None
    pr_url: str
    code_style: str
    prompt_path: pathlib.Path


def run(ctx: StageContext, options: GhreviewOptions) -> None:
    bail_section = ""
    if ctx.gr_id:
        bail_section = """

## Emit a bail marker (running under a gremlin pipeline)

After posting the review, classify your findings and — if any are blocker-severity — emit a bail marker:

- **Security-related blocker** (auth gaps, injection, credential exposure, OWASP top 10): run `python -m gremlins.bail security "<one-line summary>"`
- **Design or judgment blocker** (ambiguous requirements, architectural choices, behavior the spec doesn't pin down, security tradeoffs): run `python -m gremlins.bail reviewer_requested_changes "<one-line summary>"`
- **Do not bail** for mechanical, unambiguous fixes — missing import, wrong identifier, off-by-one, trivial wiring, dead code, single-line null check, simple rename. Flag them in the review and let the address stage handle them.

**30-second rule**: if a competent developer could write the fix in under 30 seconds without asking any questions, do not bail.

If the review has no blocker-severity findings, do not run the helper — exit normally. The bail marker is the signal the pipeline checks after this stage.
"""
    prompt = load_prompts([options.prompt_path]).format(
        pr_url=options.pr_url,
        code_style=options.code_style,
        bail_section=bail_section,
    )
    ctx.client.run(
        prompt,
        label="ghreview",
        model=options.model,
        raw_path=ctx.session_dir / "stream-ghreview.jsonl",
    )
    check_bail(ctx.gr_id, "/ghreview")


register_stage("ghreview", run)
