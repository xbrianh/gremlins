"""EngineContext: execution metadata for URI/template substitution."""

from __future__ import annotations

import dataclasses


@dataclasses.dataclass(frozen=True)
class EngineContext:
    """
    Execution metadata passed through pipeline stages.

    Not an artifact channel — carries session/loop identity so stages can
    template URIs and command strings without touching the registry.
    """

    loop_iteration: int
    attempt: str
    current_scope: tuple[str, ...]
    repo: str = ""
    cwd: str = ""

    def format(self, template: str) -> str:
        scope = "/".join(self.current_scope)
        return template.format(
            n=self.loop_iteration,
            attempt=self.attempt,
            scope=scope,
            repo=self.repo,
            cwd=self.cwd,
        )
