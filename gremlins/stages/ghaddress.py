"""ghaddress stage for the gh pipeline."""

from __future__ import annotations

import dataclasses

from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from ..state import check_bail
from .context import StageContext
from .registry import register_stage


@dataclasses.dataclass
class GhaddressOptions:
    model: str | None
    pr_url: str
    code_style: str


def run(ctx: StageContext, options: GhaddressOptions) -> None:
    bail_section = ""
    if ctx.gr_id:
        bail_section = """

## Bail markers (running under a gremlin pipeline)

If you cannot safely address one or more comments, write a bail marker before finishing — do not make speculative changes when bailing:

- Comment touches **secrets** (credential management, API keys, encryption material): `python -m gremlins.bail secrets "<one-line reason>"`
- Any other reason you decline to proceed (ambiguous ask, conflicting comments, etc.): `python -m gremlins.bail other "<one-line reason>"`

Out-of-scope comments and `gh issue create` failures are not bail reasons — handle them per the instructions above. If you successfully addressed every actionable comment, do not write a bail marker — just exit normally.
"""
    prompt = load_prompts([BUNDLED_PROMPT_DIR / "ghaddress.md"]).format(
        pr_url=options.pr_url,
        code_style=options.code_style,
        bail_section=bail_section,
    )
    ctx.client.run(
        prompt,
        label="ghaddress",
        model=options.model,
        raw_path=ctx.session_dir / "stream-ghaddress.jsonl",
    )
    check_bail(ctx.gr_id, "/ghaddress")


register_stage("ghaddress", run)
