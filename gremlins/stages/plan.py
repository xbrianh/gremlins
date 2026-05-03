"""Local plan stage.

Renders ``gremlins/prompts/plan.md`` with the user's instructions and the
target plan-file path, runs ``claude -p``, and verifies the plan file was
produced.
"""

from __future__ import annotations

import pathlib

from ..clients.claude import ClaudeClient
from ..prompts import BUNDLED_PROMPT_DIR, load_prompts


def run_plan_stage(
    *,
    client: ClaudeClient,
    plan_model: str,
    plan_file: pathlib.Path,
    instructions: str,
    raw_path: pathlib.Path,
    code_style: str,
) -> None:
    template = load_prompts([BUNDLED_PROMPT_DIR / "plan.md"])
    prompt = template.format(
        plan_file=plan_file, instructions=instructions, code_style=code_style
    )
    client.run(prompt, label="plan", model=plan_model, raw_path=raw_path)
    if not plan_file.exists() or plan_file.stat().st_size == 0:
        raise RuntimeError(f"plan stage did not produce {plan_file}")
