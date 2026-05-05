from __future__ import annotations

import dataclasses
from typing import TYPE_CHECKING, cast

from ..stages.registry import CLIENT_FACTORIES, register_client_factory
from .claude import SubprocessClaudeClient
from .copilot import SubprocessCopilotClient

if TYPE_CHECKING:
    from .protocol import ClaudeClient

register_client_factory("claude", lambda _: SubprocessClaudeClient())
register_client_factory("copilot", lambda _: SubprocessCopilotClient())


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


def resolve_stage_client(
    stage_client: ClientSpec | None,
    cli: ClientSpec | None,
    pipeline_default: ClientSpec | None,
) -> ClientSpec:
    return stage_client or cli or pipeline_default or PACKAGE_DEFAULT
