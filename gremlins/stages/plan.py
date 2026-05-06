"""Local plan stage."""

from __future__ import annotations

import pathlib
from typing import Any

from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage


class Plan(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        plan_file: pathlib.Path,
        instructions: str,
        code_style: str,
    ) -> None:
        super().__init__(entry, model)
        self.plan_file = plan_file
        self.instructions = instructions
        self.code_style = code_style

    def run(self, pipe: Any) -> None:
        template = load_prompts(self.prompt_paths)
        prompt = template.format(
            plan_file=self.plan_file,
            instructions=self.instructions,
        )
        self.run_claude(
            prompt,
            label="plan",
            raw_path=self.state.session_dir / "stream-plan.jsonl",
        )
        if not self.plan_file.exists() or self.plan_file.stat().st_size == 0:
            raise RuntimeError(f"plan stage did not produce {self.plan_file}")


register_stage("plan", Plan)
