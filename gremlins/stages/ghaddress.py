"""ghaddress stage for the gh pipeline."""

from __future__ import annotations

import dataclasses
import pathlib

from gremlins.prompts import load_prompts
from gremlins.stages.context import StageContext
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail


@dataclasses.dataclass
class GhaddressOptions:
    model: str | None
    pr_url: str
    code_style: str
    prompt_path: pathlib.Path


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
    prompt = load_prompts([options.prompt_path]).format(
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
