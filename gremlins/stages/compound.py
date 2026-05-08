from __future__ import annotations

from typing import TYPE_CHECKING

from gremlins.stages.base import Stage

if TYPE_CHECKING:
    from gremlins.pipeline import StageEntry


class CompoundStage(Stage):
    """Base for stages that own a list of child stage entries (body)."""

    def __init__(self, entry: StageEntry, model: str | None) -> None:
        super().__init__(entry, model)
        self.body = entry.body
