from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class Policy:
    bypass: bool = False
    blocks: dict[str, dict[str, Any]] = field(default_factory=lambda: {})

    def block_for(self, provider: str) -> dict[str, Any]:
        return self.blocks.get(provider, {})
