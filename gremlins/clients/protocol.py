from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass
class CompletedRun:
    exit_code: int
    text_result: str | None = None
    events: list[dict[str, Any]] | None = None
    cost_usd: float | None = None
