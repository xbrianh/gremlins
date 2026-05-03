"""ghreview stage for the gh pipeline."""

from __future__ import annotations

import dataclasses

from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from ..state import check_bail
from .context import StageContext
from .registry import register_stage


@dataclasses.dataclass
class GhreviewOptions:
    model: str | None
    pr_url: str
    code_style: str


def run(ctx: StageContext, options: GhreviewOptions) -> None:
    bail_section = ""
    if ctx.gr_id:
        bail_section = """

## Emit a bail marker (running under a gremlin pipeline)

After posting the review, classify your findings and — if any are blocker-severity — emit a bail marker:

- **Security-related blocker** (auth gaps, injection, credential exposure, OWASP top 10): run `gremlins bail security "<one-line summary>"`
- **Other blocker-severity findings** (correctness, design, or anything a human should weigh in on): run `gremlins bail reviewer_requested_changes "<one-line summary>"`

If the review has no blocker-severity findings, do not run the helper — exit normally. The bail marker is the signal the pipeline checks after this stage.
"""
    prompt = load_prompts([BUNDLED_PROMPT_DIR / "ghreview.md"]).format(
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
    check_bail("/ghreview")


register_stage("ghreview", run)
