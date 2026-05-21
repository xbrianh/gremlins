from __future__ import annotations

from gremlins.clients.claude import SubprocessClaudeClient
from gremlins.clients.copilot import SubprocessCopilotClient
from gremlins.clients.registry import register_client_factory


def _make_openai_client(model: str | None) -> object:
    from gremlins.clients.providers.openai_agents import make_openai_client

    return make_openai_client(model)


def _make_xai_client(model: str | None) -> object:
    from gremlins.clients.providers.openai_agents import make_xai_client

    return make_xai_client(model)


def _make_anthropic_client(model: str | None) -> object:
    from gremlins.clients.providers.anthropic_sdk import make_anthropic_client

    return make_anthropic_client(model)


register_client_factory("anthropic", _make_anthropic_client)
register_client_factory("claude", lambda _: SubprocessClaudeClient())
register_client_factory("copilot", lambda _: SubprocessCopilotClient())
register_client_factory("openai", _make_openai_client)
register_client_factory("xai", _make_xai_client)
