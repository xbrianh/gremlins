"""ClaudePrompt stage — run a Claude agent using accumulated prompt context."""

from __future__ import annotations

from typing import Any

from gremlins.stages.base import RuntimeState, Stage
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail


class ClaudePrompt(Stage):
    type = "claude-prompt"

    @classmethod
    def from_yaml(cls, d: dict[str, Any]) -> ClaudePrompt:
        from gremlins.pipeline.loader import _get_client_from_yaml

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = _get_client_from_yaml(d)
        return stage

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
