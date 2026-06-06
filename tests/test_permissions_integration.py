"""Integration tests: verify each backend honours its native_block at runtime."""

from __future__ import annotations

from conftest import _TestGremlin
import asyncio
import os
import shutil
from typing import Any

import pytest

from gremlins.clients.copilot import SubprocessCopilotClient
from gremlins.clients.providers.anthropic_sdk import make_anthropic_client
from gremlins.clients.providers.openai_agents import make_openai_client, make_xai_client
from gremlins.permissions.policy import Policy

_PROMPT = (
    "Run `echo gremlins-allowed` via the Bash tool. Reply with exactly the word DONE."
)

_ANTHROPIC_SKIP = pytest.mark.skipif(
    not os.environ.get("ANTHROPIC_API_KEY"), reason="ANTHROPIC_API_KEY not set"
)
_OPENAI_SKIP = pytest.mark.skipif(
    not os.environ.get("OPENAI_API_KEY"), reason="OPENAI_API_KEY not set"
)
_XAI_SKIP = pytest.mark.skipif(
    not os.environ.get("XAI_API_KEY"), reason="XAI_API_KEY not set"
)
_COPILOT_SKIP = pytest.mark.skipif(
    not shutil.which("copilot"), reason="copilot not on PATH"
)


def _bash_ran(events: list[dict[str, Any]] | None) -> bool:
    if events is None:
        return False
    for evt in events:
        if evt.get("type") == "assistant":
            for block in evt.get("message", {}).get("content", []):
                if block.get("type") == "tool_use" and block.get("name") == "Bash":
                    return True
    return False


def _bash_output_lines(events: list[dict[str, Any]] | None) -> list[str]:
    if events is None:
        return []
    lines = []
    for evt in events:
        if evt.get("type") == "user":
            for block in evt.get("message", {}).get("content", []):
                if block.get("type") == "tool_result":
                    content = block.get("content", "")
                    if isinstance(content, str):
                        lines.append(content)
    return lines


# Backends that honour native_block via event-visible tool calls.
_SDK_ALLOW_PARAMS = [
    pytest.param(
        lambda: make_anthropic_client("claude-haiku-4-5-20251001", Policy()),
        id="anthropic",
        marks=_ANTHROPIC_SKIP,
    ),
    pytest.param(
        lambda: make_openai_client("gpt-4o-mini", Policy()),
        id="openai",
        marks=_OPENAI_SKIP,
    ),
    pytest.param(
        lambda: make_xai_client("grok-3-mini-fast", Policy()),
        id="xai",
        marks=_XAI_SKIP,
    ),
]

_SDK_DENY_PARAMS = [
    pytest.param(
        lambda: make_anthropic_client(
            "claude-haiku-4-5-20251001",
            Policy(blocks={"anthropic": {"disallowed_tools": ["Bash"]}}),
        ),
        id="anthropic",
        marks=_ANTHROPIC_SKIP,
    ),
    pytest.param(
        lambda: make_openai_client(
            "gpt-4o-mini",
            Policy(blocks={"openai": {"allowed_tools": ["Read"]}}),
        ),
        id="openai",
        marks=_OPENAI_SKIP,
    ),
    pytest.param(
        lambda: make_xai_client(
            "grok-3-mini-fast",
            Policy(blocks={"xai": {"allowed_tools": ["Read"]}}),
        ),
        id="xai",
        marks=_XAI_SKIP,
    ),
]


@pytest.mark.integration
@pytest.mark.parametrize("make_client", _SDK_ALLOW_PARAMS)
def test_default_block_allows_standard_toolset(make_client: Any) -> None:
    client = make_client()
    result = asyncio.run(client.run(_TestGremlin(_PROMPT, label="perm-allow", capture_events=True)))
    assert result.exit_code == 0
    assert result.text_result and "DONE" in result.text_result
    assert _bash_ran(result.events), "Bash was not called"
    assert any(
        "gremlins-allowed" in line for line in _bash_output_lines(result.events)
    ), "gremlins-allowed not in Bash output"


@pytest.mark.integration
@_COPILOT_SKIP
def test_default_block_allows_standard_toolset_copilot() -> None:
    client = SubprocessCopilotClient(bypass=True, native_block={})
    result = asyncio.run(client.run(_TestGremlin(_PROMPT, label="perm-allow")))
    assert result.exit_code == 0
    assert result.text_result and "DONE" in result.text_result


@pytest.mark.integration
@pytest.mark.parametrize("make_client", _SDK_DENY_PARAMS)
def test_override_block_denies_disallowed_tool(make_client: Any) -> None:
    client = make_client()
    result = asyncio.run(client.run(_TestGremlin(_PROMPT, label="perm-deny", capture_events=True)))
    assert not _bash_ran(result.events), "Bash ran despite being blocked"
    assert not any(
        "gremlins-allowed" in line for line in _bash_output_lines(result.events)
    )


@pytest.mark.integration
@_COPILOT_SKIP
@pytest.mark.xfail(
    reason=(
        "copilot CLI has no per-tool allowlist surface; native_block cannot be "
        "expressed as argv — see gremlins/clients/AGENTS.md"
    )
)
def test_override_block_denies_disallowed_tool_copilot() -> None:
    client = SubprocessCopilotClient(bypass=True, native_block={})
    result = asyncio.run(client.run(_TestGremlin(_PROMPT, label="perm-deny")))
    assert result.text_result and "gremlins-allowed" not in result.text_result
