from __future__ import annotations

from collections.abc import Callable
from typing import Any

STAGE_REGISTRY: dict[str, Callable[..., Any]] = {}
CLIENT_FACTORIES: dict[str, Callable[[str | None], Any]] = {}
STAGE_BUILDERS: dict[str, Callable[..., Any]] = {}
STAGE_NEEDS_PIPE: dict[str, bool] = {}


def register_stage(name: str, fn: Callable[..., Any]) -> None:
    STAGE_REGISTRY[name] = fn


def register_client_factory(
    provider: str, factory: Callable[[str | None], Any]
) -> None:
    CLIENT_FACTORIES[provider] = factory


def register_stage_builder(
    name: str,
    builder: Callable[..., Any],
    *,
    needs_pipe: bool = False,
) -> None:
    STAGE_BUILDERS[name] = builder
    STAGE_NEEDS_PIPE[name] = needs_pipe
