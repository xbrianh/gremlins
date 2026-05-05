from __future__ import annotations

import pathlib
from dataclasses import dataclass
from typing import Any, Protocol


@dataclass
class CompletedRun:
    exit_code: int
    session_id: str | None = None
    text_result: str | None = None
    events: list[dict[str, Any]] | None = None
    cost_usd: float | None = None


class ClaudeClient(Protocol):
    def run(
        self,
        prompt: str,
        *,
        label: str,
        model: str | None = None,
        raw_path: pathlib.Path | None = None,
        capture_events: bool = False,
        on_timeout_prompt: str | None = None,
        max_retries: int = 2,
    ) -> CompletedRun: ...

    def reap_all(self) -> None: ...

    @property
    def total_cost_usd(self) -> float: ...
