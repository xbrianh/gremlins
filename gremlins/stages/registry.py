from __future__ import annotations

from collections.abc import Callable
from typing import Any

STAGE_REGISTRY: dict[str, Callable[..., Any]] = {}
STAGE_BUILDERS: dict[str, Callable[..., Any]] = {}


def register_stage(name: str, fn: Callable[..., Any]) -> None:
    STAGE_REGISTRY[name] = fn


def register_stage_builder(
    name: str,
    builder: Callable[..., Any],
) -> None:
    STAGE_BUILDERS[name] = builder
