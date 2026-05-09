from __future__ import annotations

from .base import Stage


class CompoundStage(Stage):
    """Base for stages that own child stages."""

    def __init__(self, name: str) -> None:
        super().__init__(name, None, [], {})
