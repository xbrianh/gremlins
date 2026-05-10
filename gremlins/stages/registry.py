from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from gremlins.stages.base import Stage

STAGE_REGISTRY: dict[str, type[Stage]] = {}


def register_stage(name: str, stage_cls: type[Stage]) -> None:
    STAGE_REGISTRY[name] = stage_cls
