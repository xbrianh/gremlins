"""ClaudePrompt stage — run a Claude agent using accumulated prompt context."""

from __future__ import annotations

from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage


class ClaudePrompt(Stage):
    type = "claude-prompt"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> ClaudePrompt:
        from gremlins.pipeline.loader import get_client_from_dict

        prompts: list[str] = d.get("prompt") or []
        if not prompts:
            raise ValueError(f"stage {d['name']!r}: 'prompt' is required")
        stage = cls(d["name"], None, prompts, d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> None:
        prompt = "\n\n".join(self.prompts).rstrip()
        self.run_claude(
            prompt,
            state=state,
            label=self.name,
            raw_path=state.session_dir / f"stream-{self.name}.jsonl",
        )
        state.check_bail(self.name)


register_stage("claude-prompt", ClaudePrompt)
