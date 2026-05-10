from __future__ import annotations

from collections.abc import Callable
from typing import Any

CLIENT_FACTORIES: dict[str, Callable[[str | None], Any]] = {}


def register_client_factory(
    provider: str, factory: Callable[[str | None], Any]
) -> None:
    CLIENT_FACTORIES[provider] = factory
