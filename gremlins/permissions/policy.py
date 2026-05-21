from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

KNOWN_PROVIDERS = ("claude", "copilot", "openai", "xai", "anthropic")


@dataclass(frozen=True)
class Policy:
    bypass: bool = False
    blocks: dict[str, dict[str, Any]] = field(default_factory=dict[str, dict[str, Any]])

    @classmethod
    def empty(cls) -> Policy:
        return cls()

    def block_for(self, provider: str) -> dict[str, Any]:
        return self.blocks.get(provider, {})
