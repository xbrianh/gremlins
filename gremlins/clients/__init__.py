from __future__ import annotations

from typing import cast

from gremlins.clients import resolve as _resolve
from gremlins.clients.claude import SubprocessClaudeClient
from gremlins.clients.copilot import SubprocessCopilotClient
from gremlins.clients.protocol import ClaudeClient, CompletedRun
from gremlins.clients.stream import stream_events
from gremlins.stages.registry import CLIENT_FACTORIES, register_client_factory

__all__ = [
    "ClaudeClient",
    "ClientSpec",
    "CompletedRun",
    "PACKAGE_DEFAULT",
    "collect_stage_specs",
    "load_stage_specs_from_state",
    "require_stage_spec",
    "resolve_stage_client",
    "stream_events",
    "to_client",
    "validate_stage_specs",
]


def _make_openai_client(model: str | None) -> object:
    from gremlins.clients.providers.openai_agents import make_openai_client

    return make_openai_client(model)


def _make_xai_client(model: str | None) -> object:
    from gremlins.clients.providers.openai_agents import make_xai_client

    return make_xai_client(model)


register_client_factory("claude", lambda _: SubprocessClaudeClient())
register_client_factory("copilot", lambda _: SubprocessCopilotClient())
register_client_factory("openai", _make_openai_client)
register_client_factory("xai", _make_xai_client)

PACKAGE_DEFAULT = _resolve.PACKAGE_DEFAULT
ClientSpec = _resolve.ClientSpec
collect_stage_specs = _resolve.collect_stage_specs
load_stage_specs_from_state = _resolve.load_stage_specs_from_state
require_stage_spec = _resolve.require_stage_spec
resolve_stage_client = _resolve.resolve_stage_client
validate_stage_specs = _resolve.validate_stage_specs


def to_client(spec: ClientSpec) -> ClaudeClient:
    return cast("ClaudeClient", CLIENT_FACTORIES[spec.provider](spec.model or None))
