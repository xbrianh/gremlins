"""ClaudePrompt stage — run a Claude agent using accumulated prompt context."""

from __future__ import annotations

from gremlins.stages.base import Stage, RuntimeState
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail


class ClaudePrompt(Stage):
    def run(self, state: RuntimeState) -> None:
        prompt = "\n\n".join(self.prompts).rstrip()
        self.run_claude(
            prompt,
            state=state,
            label=self.name,
            raw_path=state.session_dir / f"stream-{self.name}.jsonl",
        )
        check_bail(state.gr_id, self.name, child_key=state.child_key)


register_stage("claude-prompt", ClaudePrompt)
