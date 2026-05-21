from __future__ import annotations

from collections.abc import Callable
from typing import Any

from gremlins.permissions.policy import Policy

CLIENT_FACTORIES: dict[str, Callable[[str | None, Policy], Any]] = {}


def register_client_factory(
    provider: str,
    factory: Callable[[str | None, Policy], Any],
) -> None:
    CLIENT_FACTORIES[provider] = factory
