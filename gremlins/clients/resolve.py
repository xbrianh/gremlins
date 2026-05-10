from __future__ import annotations

import dataclasses
from typing import TYPE_CHECKING, cast

from gremlins.clients.registry import CLIENT_FACTORIES

if TYPE_CHECKING:
    from gremlins.clients.protocol import ClaudeClient


@dataclasses.dataclass(frozen=True)
class ClientSpec:
    provider: str
    model: str

    @staticmethod
    def parse(spec: str) -> ClientSpec:
        if ":" not in spec:
            raise ValueError(
                f"invalid client specifier {spec!r}: expected 'provider:model'"
            )
        provider, _, model = spec.partition(":")
        if not provider:
            raise ValueError(
                f"invalid client specifier {spec!r}: expected 'provider:model'"
            )
        if not model:
            raise ValueError(
                f"invalid client specifier {spec!r}: model must not be empty"
            )
        if provider not in CLIENT_FACTORIES:
            raise ValueError(
                f"unknown provider {provider!r} in client specifier {spec!r}"
            )
        return ClientSpec(provider=provider, model=model)

    def __str__(self) -> str:
        return f"{self.provider}:{self.model}"


PACKAGE_DEFAULT = ClientSpec("claude", "sonnet")


def to_client(spec: ClientSpec) -> ClaudeClient:
    return cast("ClaudeClient", CLIENT_FACTORIES[spec.provider](spec.model or None))
