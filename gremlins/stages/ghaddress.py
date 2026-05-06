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
    ) -> None:
        super().__init__(entry, model)
        self.pr_url = pr_url

    def run(self, pipe: Any) -> None:
        prompt = load_prompts(self.prompt_paths).format(
            pr_url=self.pr_url,
        )
        self.run_claude(
            prompt,
            label="ghaddress",
            raw_path=self.state.session_dir / "stream-ghaddress.jsonl",
        )
        check_bail(self.state.gr_id, "/ghaddress", child_key=self.state.child_key)


register_stage("ghaddress", GHAddress)
