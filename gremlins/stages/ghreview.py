"""ghreview stage for the gh pipeline."""

from __future__ import annotations

from typing import Any

from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail


class GHReview(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        pr_url: str,
    ) -> None:
        super().__init__(entry, model)
        self.pr_url = pr_url

    def run(self, pipe: Any) -> None:
        bail_section = ""
        if self.state.gr_id:
            bail_section = """

## Emit a bail marker (running under a gremlin pipeline)

After posting the review, classify your findings and — if any are blocker-severity — emit a bail marker:

The only question that matters: **can the address stage fix this without asking anyone?** If yes, do not bail — flag it in the review and move on.

- **Security blocker** (auth gaps, injection, credential exposure, OWASP top 10): run `python -m gremlins.bail security "<one-line summary>"`
- **Unfixable blocker** — the address stage cannot proceed because the spec is ambiguous, the approach is fundamentally wrong, or the required behavior is a judgment call not pinned down by the issue: run `python -m gremlins.bail reviewer_requested_changes "<one-line summary>"`
- **Everything else**: do not bail. Incomplete wiring, missing imports, dead code, wrong identifiers, off-by-ones, missing tests, simple renames — flag them and let the address stage handle them. Err strongly on the side of not bailing.

If the review has no blocker-severity findings, do not run the helper — exit normally. The bail marker is the signal the pipeline checks after this stage.

**30-second rule**: if a competent developer could fix it in under 30 seconds without asking questions — missing import, wrong identifier, off-by-one, trivial rename — do not bail; flag it in the review.
"""
        prompt = load_prompts(self.prompt_paths).format(
            pr_url=self.pr_url,
            bail_section=bail_section,
        )
        self.run_claude(
            prompt,
            label="ghreview",
            raw_path=self.state.session_dir / "stream-ghreview.jsonl",
        )
        check_bail(self.state.gr_id, "/ghreview", child_key=self.state.child_key)


register_stage("ghreview", GHReview)
