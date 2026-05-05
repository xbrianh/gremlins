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

The only question that matters: **can the address stage fix this without asking anyone?** If yes, do not bail — flag it in the review and move on.

- **Security blocker** (auth gaps, injection, credential exposure, OWASP top 10): run `python -m gremlins.bail security "<one-line summary>"`
- **Unfixable blocker** — the address stage cannot proceed because the spec is ambiguous, the approach is fundamentally wrong, or the required behavior is a judgment call not pinned down by the issue: run `python -m gremlins.bail reviewer_requested_changes "<one-line summary>"`
- **Everything else**: do not bail. Incomplete wiring, missing imports, dead code, wrong identifiers, off-by-ones, missing tests, simple renames — flag them and let the address stage handle them. Err strongly on the side of not bailing.

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
