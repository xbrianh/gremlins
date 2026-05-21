from __future__ import annotations

from gremlins.clients.claude import SubprocessClaudeClient
from gremlins.clients.copilot import SubprocessCopilotClient
from gremlins.clients.registry import register_client_factory
from gremlins.permissions.policy import Policy


def _make_claude_client(_model: str | None, policy: Policy) -> SubprocessClaudeClient:
    return SubprocessClaudeClient(
        bypass=policy.bypass,
        native_block=policy.block_for("claude"),
    )


def _make_copilot_client(_model: str | None, policy: Policy) -> SubprocessCopilotClient:
    return SubprocessCopilotClient(
        bypass=policy.bypass,
        native_block=policy.block_for("copilot"),
    )


def _make_openai_client(model: str | None, policy: Policy) -> object:
    from gremlins.clients.providers.openai_agents import make_openai_client

    return make_openai_client(model, policy)


def _make_xai_client(model: str | None, policy: Policy) -> object:
    from gremlins.clients.providers.openai_agents import make_xai_client

    return make_xai_client(model, policy)


def _make_anthropic_client(model: str | None, policy: Policy) -> object:
    from gremlins.clients.providers.anthropic_sdk import make_anthropic_client

    return make_anthropic_client(model, policy)


register_client_factory(
    "anthropic", _make_anthropic_client, takes_permission_block=True
)
register_client_factory("claude", _make_claude_client, takes_permission_block=True)
register_client_factory("copilot", _make_copilot_client, takes_permission_block=True)
register_client_factory("openai", _make_openai_client, takes_permission_block=True)
register_client_factory("xai", _make_xai_client, takes_permission_block=True)
