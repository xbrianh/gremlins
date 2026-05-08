"""ClaudePrompt stage — run a Claude agent using accumulated prompt context."""

from __future__ import annotations

from typing import Any

from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail


class ClaudePrompt(Stage):
    def run(self, pipe: Any) -> None:  # noqa: ARG002
        prompt = load_prompts(self.prompt_paths)
        self.run_claude(
            prompt,
            label=self.name,
            raw_path=self.state.session_dir / f"stream-{self.name}.jsonl",
        )
        check_bail(self.state.gr_id, self.name, child_key=self.state.child_key)


register_stage("claude-prompt", ClaudePrompt)
