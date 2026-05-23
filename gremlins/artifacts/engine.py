"""EngineContext: loop/attempt metadata for URI template substitution."""
from __future__ import annotations

import dataclasses


@dataclasses.dataclass(frozen=True)
class EngineContext:
    """
    Execution metadata passed through pipeline stages.

    Not an artifact channel — carries loop/attempt identity so stages can
    template URIs like ``file://session/handoff-{n}.md`` without needing
    the registry.
    """

    loop_iteration: int
    attempt: str
    current_scope: list[str]

    def format(self, template: str) -> str:
        scope = "/".join(self.current_scope)
        return template.format(n=self.loop_iteration, attempt=self.attempt, scope=scope)
