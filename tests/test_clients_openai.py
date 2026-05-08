"""Tests for OpenAIAgentsClient and GREMLINS_TOOLS."""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

import pytest
from agents import Usage

from gremlins.clients.protocol import CompletedRun
from gremlins.clients.providers.openai_agents import OpenAIAgentsClient, make_xai_client
from gremlins.clients.tools import GREMLINS_TOOLS


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
    assert client._provider is not None


def test_xai_client_missing_key(monkeypatch: Any) -> None:
    monkeypatch.delenv("XAI_API_KEY", raising=False)
    with pytest.raises(RuntimeError):
        make_xai_client(None)
