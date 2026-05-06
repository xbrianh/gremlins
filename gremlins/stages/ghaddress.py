"""ghaddress stage for the gh pipeline."""

from __future__ import annotations

from typing import Any

from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail


class GHAddress(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        pr_url: str,
        code_style: str,
    ) -> None:
        super().__init__(entry, model)
        self.pr_url = pr_url
        self.code_style = code_style

    def run(self, pipe: Any) -> None:
        bail_section = ""
        if self.state.gr_id:
            bail_section = """

## Bail markers (running under a gremlin pipeline)

If you cannot safely address one or more comments, write a bail marker before finishing — do not make speculative changes when bailing:

- Comment touches **secrets** (credential management, API keys, encryption material): `python -m gremlins.bail secrets "<one-line reason>"`
- Any other reason you decline to proceed (ambiguous ask, conflicting comments, etc.): `python -m gremlins.bail other "<one-line reason>"`

Out-of-scope comments and `gh issue create` failures are not bail reasons — handle them per the instructions above. If you successfully addressed every actionable comment, do not write a bail marker — just exit normally.
"""
        prompt_path = self.prompt_paths[-1]
        prompt = load_prompts([prompt_path]).format(
            pr_url=self.pr_url,
            code_style=self.code_style,
            bail_section=bail_section,
        )
        self.run_claude(
            prompt,
            label="ghaddress",
            raw_path=self.state.session_dir / "stream-ghaddress.jsonl",
        )
        check_bail(self.state.gr_id, "/ghaddress", child_key=self.state.child_key)


register_stage("ghaddress", GHAddress)
