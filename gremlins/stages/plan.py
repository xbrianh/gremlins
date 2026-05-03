"""Local plan stage."""

from __future__ import annotations

import dataclasses
import pathlib

from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from .context import StageContext


@dataclasses.dataclass
class PlanOptions:
    plan_model: str
    plan_file: pathlib.Path
    instructions: str
    code_style: str


def run(ctx: StageContext, options: PlanOptions) -> None:
    template = load_prompts([BUNDLED_PROMPT_DIR / "plan.md"])
    prompt = template.format(
        plan_file=options.plan_file,
        instructions=options.instructions,
        code_style=options.code_style,
    )
    ctx.client.run(
        prompt,
        label="plan",
        model=options.plan_model,
        raw_path=ctx.session_dir / "stream-plan.jsonl",
    )
    if not options.plan_file.exists() or options.plan_file.stat().st_size == 0:
        raise RuntimeError(f"plan stage did not produce {options.plan_file}")
