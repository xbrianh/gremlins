from __future__ import annotations

import dataclasses
import json
from typing import TYPE_CHECKING, cast

from ..stages.registry import CLIENT_FACTORIES, register_client_factory
from ..state import resolve_state_file
from .claude import SubprocessClaudeClient
from .copilot import SubprocessCopilotClient

if TYPE_CHECKING:
    from ..pipeline import Pipeline
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


def resolve_stage_client(
    stage_client: ClientSpec | None,
    cli: ClientSpec | None,
    pipeline_default: ClientSpec | None,
) -> ClientSpec:
    return stage_client or cli or pipeline_default or PACKAGE_DEFAULT


def collect_stage_specs(
    pipeline: Pipeline,
    cli_spec: ClientSpec | None,
) -> dict[str, ClientSpec]:
    specs: dict[str, ClientSpec] = {}
    for e in pipeline.stages:
        if e.type == "parallel":
            specs[e.name] = resolve_stage_client(
                None, cli_spec, pipeline.default_client
            )
            for child in e.children:
                specs[child.name] = resolve_stage_client(
                    child.client, cli_spec, pipeline.default_client
                )
        else:
            specs[e.name] = resolve_stage_client(
                e.client, cli_spec, pipeline.default_client
            )
    return specs


def load_stage_specs_from_state(gr_id: str | None) -> dict[str, ClientSpec]:
    """Return the stage→spec map persisted in state.json, or {} if absent.

    Raises ValueError if stage_clients exists but contains an unparseable spec,
    or OSError/json.JSONDecodeError if the state file is malformed.
    """
    if not gr_id:
        return {}
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return {}
    data = json.loads(sf.read_text(encoding="utf-8"))
    stored = data.get("stage_clients", {})
    return {str(k): ClientSpec.parse(str(v)) for k, v in stored.items()}
