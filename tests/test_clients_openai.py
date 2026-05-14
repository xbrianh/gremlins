"""Tests for OpenAIAgentsClient and GREMLINS_TOOLS."""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from agents import Usage
from agents.tool_context import ToolContext

from gremlins.clients.protocol import CompletedRun
from gremlins.clients.providers.openai_agents import (
    OpenAIAgentsClient,
    make_openai_client,
    make_xai_client,
)
from gremlins.clients.tools import GREMLINS_TOOLS, _bash_invoke


def test_openai_client_constructs() -> None:
    client = OpenAIAgentsClient("gpt-4o")
    assert client.total_cost_usd == 0.0


def test_gremlins_tools_nonempty() -> None:
    assert len(GREMLINS_TOOLS) > 0
    for tool in GREMLINS_TOOLS:
        assert hasattr(tool, "name") and tool.name
        assert hasattr(tool, "params_json_schema") and isinstance(
            tool.params_json_schema, dict
        )


def _make_fake_result(text: str, input_tokens: int, output_tokens: int) -> Any:
    usage = Usage(
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        total_tokens=input_tokens + output_tokens,
        requests=1,
    )
    ctx_wrapper = MagicMock()
    ctx_wrapper.usage = usage
    result = MagicMock()
    result.final_output = text
    result.context_wrapper = ctx_wrapper
    return result


def test_fake_completion_produces_completed_run(monkeypatch: Any) -> None:
    fake_result = _make_fake_result("Done.", input_tokens=100, output_tokens=50)
    monkeypatch.setattr("agents.run.Runner.run_sync", lambda *a, **kw: fake_result)

    client = OpenAIAgentsClient("gpt-4o")
    result = client.run("do something", label="test")

    assert isinstance(result, CompletedRun)
    assert result.exit_code == 0
    assert result.text_result == "Done."
    assert result.cost_usd is not None
    assert result.cost_usd > 0
    assert client.total_cost_usd == result.cost_usd


def test_xai_client_constructs(monkeypatch: Any) -> None:
    monkeypatch.setenv("XAI_API_KEY", "test-key")
    client = make_xai_client(None)
    assert isinstance(client, OpenAIAgentsClient)
    assert client._model == "grok-4"
    assert client.base_url == "https://api.x.ai/v1"
    assert client.api_key == "test-key"


def test_xai_client_missing_key(monkeypatch: Any) -> None:
    monkeypatch.delenv("XAI_API_KEY", raising=False)
    with pytest.raises(RuntimeError):
        make_xai_client(None)


def test_openai_client_constructs_with_api_key(monkeypatch: Any) -> None:
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    client = make_openai_client(None)
    assert isinstance(client, OpenAIAgentsClient)
    assert client.api_key == "sk-test"


def test_openai_client_missing_key(monkeypatch: Any) -> None:
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    with pytest.raises(RuntimeError):
        make_openai_client(None)


@pytest.mark.skipif(
    not os.environ.get("OPENAI_API_KEY"),
    reason="OPENAI_API_KEY not set",
)
def test_openai_integration_run() -> None:
    client = make_openai_client("gpt-4o-mini")
    result = client.run("Reply with the single word: done", label="integration-test")
    assert result.exit_code == 0
    assert result.text_result


@pytest.mark.skipif(
    not os.environ.get("XAI_API_KEY"),
    reason="XAI_API_KEY not set",
)
def test_xai_integration_run() -> None:
    client = make_xai_client("grok-3-mini-fast")
    result = client.run("Reply with the single word: done", label="integration-test")
    assert result.exit_code == 0
    assert result.text_result


def _make_tool_ctx(context: dict[str, Any]) -> ToolContext[Any]:
    return ToolContext(
        context=context,
        tool_name="Bash",
        tool_call_id="call_1",
        tool_arguments='{"command": "echo hi"}',
    )


def test_bash_invoke_passes_extra_env_to_subprocess() -> None:
    fake_proc = MagicMock()
    fake_proc.communicate = AsyncMock(return_value=(b"hi\n", None))
    fake_proc.returncode = 0

    ctx = _make_tool_ctx({"extra_env": {"MY_TOKEN": "abc123"}})
    args_json = json.dumps({"command": "echo hi"})

    with patch("asyncio.create_subprocess_shell", return_value=fake_proc) as mock_spawn:
        asyncio.run(_bash_invoke(ctx, args_json))

    _call_kwargs = mock_spawn.call_args.kwargs
    assert "env" in _call_kwargs
    env = _call_kwargs["env"]
    assert env["MY_TOKEN"] == "abc123"
    assert "PATH" in env  # inherited from os.environ


def test_bash_invoke_no_extra_env_passes_none() -> None:
    fake_proc = MagicMock()
    fake_proc.communicate = AsyncMock(return_value=(b"hi\n", None))
    fake_proc.returncode = 0

    ctx = _make_tool_ctx({})
    args_json = json.dumps({"command": "echo hi"})

    with patch("asyncio.create_subprocess_shell", return_value=fake_proc) as mock_spawn:
        asyncio.run(_bash_invoke(ctx, args_json))

    _call_kwargs = mock_spawn.call_args.kwargs
    assert _call_kwargs.get("env") is None
