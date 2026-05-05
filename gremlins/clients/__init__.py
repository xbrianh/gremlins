from __future__ import annotations

from typing import TYPE_CHECKING, cast

from ..stages.registry import CLIENT_FACTORIES, register_client_factory
from . import resolve as _resolve
from .claude import SubprocessClaudeClient
from .copilot import SubprocessCopilotClient

if TYPE_CHECKING:
    from .protocol import ClaudeClient

register_client_factory("claude", lambda _: SubprocessClaudeClient())
register_client_factory("copilot", lambda _: SubprocessCopilotClient())

PACKAGE_DEFAULT = _resolve.PACKAGE_DEFAULT
ClientSpec = _resolve.ClientSpec
collect_stage_specs = _resolve.collect_stage_specs
load_stage_specs_from_state = _resolve.load_stage_specs_from_state
require_stage_spec = _resolve.require_stage_spec
resolve_stage_client = _resolve.resolve_stage_client


def to_client(spec: ClientSpec) -> ClaudeClient:
    return cast("ClaudeClient", CLIENT_FACTORIES[spec.provider](spec.model or None))
