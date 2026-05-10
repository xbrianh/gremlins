from __future__ import annotations

from collections.abc import Callable
from typing import Any

STAGE_REGISTRY: dict[str, Callable[..., Any]] = {}


def register_stage(name: str, fn: Callable[..., Any]) -> None:
    STAGE_REGISTRY[name] = fn
