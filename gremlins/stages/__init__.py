from __future__ import annotations

from typing import TYPE_CHECKING

from gremlins.stages.base import Stage, StageContext, StageInput
from gremlins.stages.registry import (
    STAGE_BUILDERS,
    STAGE_NEEDS_PIPE,
    STAGE_REGISTRY,
    register_stage,
    register_stage_builder,
)

if TYPE_CHECKING:
    from gremlins.stages.compound import CompoundStage
    from gremlins.stages.loop import LoopExhausted, LoopStage, RunCmdFailed

__all__ = [
    "CompoundStage",
    "LoopExhausted",
    "LoopStage",
    "RunCmdFailed",
    "Stage",
    "StageContext",
    "StageInput",
    "STAGE_BUILDERS",
    "STAGE_NEEDS_PIPE",
    "STAGE_REGISTRY",
    "register_stage",
    "register_stage_builder",
]

_LAZY_COMPOUND = ("CompoundStage",)
_LAZY_LOOP = ("LoopExhausted", "LoopStage", "RunCmdFailed")


def __getattr__(name: str) -> object:
    if name in _LAZY_COMPOUND:
        import gremlins.stages.compound as _m

        val = getattr(_m, name)
        globals()[name] = val
        return val
    if name in _LAZY_LOOP:
        import gremlins.stages.loop as _m

        val = getattr(_m, name)
        globals()[name] = val
        return val
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
