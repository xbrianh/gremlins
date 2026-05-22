from __future__ import annotations

from collections.abc import Callable
from typing import Any

from gremlins.permissions.policy import Policy

CLIENT_FACTORIES: dict[str, Callable[[str | None, Policy], Any]] = {}
BYPASS_REQUIRED: set[str] = set()  # providers that only work with bypass=True


def register_client_factory(
    provider: str,
    factory: Callable[[str | None, Policy], Any],
    *,
    bypass_only: bool = False,
) -> None:
    CLIENT_FACTORIES[provider] = factory
    if bypass_only:
        BYPASS_REQUIRED.add(provider)
