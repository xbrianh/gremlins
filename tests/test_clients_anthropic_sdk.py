from __future__ import annotations

import asyncio
import sys
from dataclasses import dataclass, field
from types import SimpleNamespace
from typing import Any

import pytest

from gremlins.clients.client import Client
from gremlins.clients.protocol import CompletedRun
from gremlins.clients.providers.anthropic_sdk import (
    AnthropicSdkClient,
    StreamTerminalError,
    StreamTimeoutError,
    make_anthropic_client,
)
from gremlins.permissions.policy import Policy

# ---------------------------------------------------------------------------
# Stub types that mirror the real claude_agent_sdk message types.
# The implementation dispatches on type(msg).__name__, so these just need
# the right class name and the right attributes.
# ---------------------------------------------------------------------------


@dataclass
class TextBlock:
    text: str


@dataclass
class ToolUseBlock:
    id: str
    name: str
    input: dict[str, Any]


@dataclass
class ToolResultBlock:
    tool_use_id: str
    content: Any = None
    is_error: Any = None


@dataclass
class AssistantMessage:
    content: list[Any]
    model: str = "test-model"


@dataclass
class UserMessage:
    content: Any


@dataclass
class ResultMessage:
    subtype: str
    duration_ms: int
    duration_api_ms: int
    is_error: bool
    num_turns: int
    session_id: str
    result: Any = None
    total_cost_usd: Any = None
    input_tokens: Any = None
    output_tokens: Any = None


@dataclass
class ClaudeAgentOptions:
    model: Any = None
    cwd: Any = None
    permission_mode: Any = None
    setting_sources: Any = None
    mcp_servers: Any = field(default_factory=dict)
    hooks: Any = None
    env: Any = field(default_factory=dict)
    allowed_tools: Any = None


# ---------------------------------------------------------------------------
# Fixture: inject stub claude_agent_sdk into sys.modules so the lazy imports
# inside run() pick up our stubs instead of the real (broken) package.
# ---------------------------------------------------------------------------


@pytest.fixture()
def mock_sdk(monkeypatch):
    async def _default_query(*, prompt: Any, options: Any):
        yield ResultMessage(
            subtype="success",
            duration_ms=100,
            duration_api_ms=80,
            is_error=False,
            num_turns=1,
            session_id="test",
            result="ok",
        )

    stub = SimpleNamespace(
        ClaudeAgentOptions=ClaudeAgentOptions,
        query=_default_query,
    )
    # Remove real module if cached, inject stub
    monkeypatch.setitem(sys.modules, "claude_agent_sdk", stub)
    return stub


# ---------------------------------------------------------------------------
# Constructor tests (don't need the SDK)
# ---------------------------------------------------------------------------


def test_constructor_missing_key_raises(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    with pytest.raises(RuntimeError, match="ANTHROPIC_API_KEY"):
        AnthropicSdkClient("claude-sonnet-4-6")


def test_constructor_with_key_succeeds(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    assert client._model == "claude-sonnet-4-6"


def test_client_has_run_method(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    assert callable(getattr(client, "run", None))


def test_client_has_reap_all(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    client.reap_all()  # must not raise


def test_client_total_cost_usd(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    assert client.total_cost_usd is None


def test_factory_constructs_client(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = make_anthropic_client("claude-sonnet-4-7", Policy())
    assert isinstance(client, AnthropicSdkClient)
    assert client._model == "claude-sonnet-4-7"


def test_factory_missing_key_raises(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    with pytest.raises(RuntimeError, match="ANTHROPIC_API_KEY"):
        make_anthropic_client("claude-sonnet-4-7", Policy())


def test_registered_factory_parses_url(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    c = Client.parse("anthropic:claude-sonnet-4-7")
    assert c.provider == "anthropic"
    assert c.model == "claude-sonnet-4-7"
    impl = c._get_impl()
    assert isinstance(impl, AnthropicSdkClient)
    assert impl._model == "claude-sonnet-4-7"


def test_registered_factory_missing_key_fails_fast(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    c = Client.parse("anthropic:claude-sonnet-4-7")
    with pytest.raises(RuntimeError, match="ANTHROPIC_API_KEY"):
        c._get_impl()


# ---------------------------------------------------------------------------
# Smoke tests
# ---------------------------------------------------------------------------


def test_run_smoke_captures_events(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")

    async def _query(*, prompt: Any, options: Any):
        yield AssistantMessage(content=[TextBlock(text="hello world")])
        yield ResultMessage(
            subtype="success",
            duration_ms=100,
            duration_api_ms=80,
            is_error=False,
            num_turns=2,
            session_id="s",
            result="hello world",
        )

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result: CompletedRun = asyncio.run(
        client.run("do something", label="smoke", capture_events=True)
    )

    assert result.exit_code == 0
    assert result.text_result == "hello world"
    assert result.events is not None
    assert any(e.get("type") == "assistant" for e in result.events)
    assert any(e.get("type") == "result" for e in result.events)


def test_run_smoke_no_capture(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result: CompletedRun = asyncio.run(
        client.run("do something", label="smoke", capture_events=False)
    )
    assert result.exit_code == 0
    assert result.events is None


def test_run_error_result_sets_exit_code(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")

    async def _error_query(*, prompt: Any, options: Any):
        yield ResultMessage(
            subtype="error",
            duration_ms=10,
            duration_api_ms=8,
            is_error=True,
            num_turns=0,
            session_id="s",
            result="oops",
        )

    mock_sdk.query = _error_query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result: CompletedRun = asyncio.run(
        client.run("do something", label="err", capture_events=False)
    )
    assert result.exit_code == 1


def test_run_tool_use_captured(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")

    async def _query(*, prompt: Any, options: Any):
        yield AssistantMessage(
            content=[
                ToolUseBlock(id="t1", name="Read", input={"file_path": "/foo.py"}),
            ]
        )
        yield UserMessage(
            content=[ToolResultBlock(tool_use_id="t1", content="file contents")]
        )
        yield ResultMessage(
            subtype="success",
            duration_ms=100,
            duration_api_ms=80,
            is_error=False,
            num_turns=1,
            session_id="s",
            result="done",
        )

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result: CompletedRun = asyncio.run(
        client.run("do something", label="t", capture_events=True)
    )

    assert result.events is not None
    tool_events = [e for e in result.events if e.get("type") == "assistant"]
    assert tool_events
    content = tool_events[0]["message"]["content"]
    assert any(c["type"] == "tool_use" and c["name"] == "Read" for c in content)


# ---------------------------------------------------------------------------
# Hermeticity tests
# ---------------------------------------------------------------------------


def _capturing_query(captured: list[Any]):
    async def _query(*, prompt: Any, options: Any):
        captured.append(options)
        yield ResultMessage(
            subtype="success",
            duration_ms=10,
            duration_api_ms=8,
            is_error=False,
            num_turns=1,
            session_id="s",
            result="ok",
        )

    return _query


def test_hermeticity_scrubs_claude_vars(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")
    monkeypatch.setenv("ANTHROPIC_BASE_URL", "junk")
    monkeypatch.setenv("CLAUDE_FOO", "1")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6")
    asyncio.run(client.run("hello", label="t"))

    assert captured, "query was not called"
    env = captured[0].env
    assert "CLAUDE_FOO" not in env
    assert "ANTHROPIC_BASE_URL" not in env
    assert env.get("ANTHROPIC_API_KEY") == "valid-key"


def test_hermeticity_extra_env_layered(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")
    monkeypatch.delenv("MY_VAR", raising=False)

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6")
    asyncio.run(client.run("hello", label="t", extra_env={"MY_VAR": "42"}))

    assert captured[0].env.get("MY_VAR") == "42"


def test_setting_sources_empty(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6")
    asyncio.run(client.run("hello", label="t"))

    assert captured[0].setting_sources == []


def test_permission_mode_bypass_true(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6", bypass=True)
    asyncio.run(client.run("hello", label="t"))

    assert captured[0].permission_mode == "bypassPermissions"


def test_permission_mode_bypass_false(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6", bypass=False)
    asyncio.run(client.run("hello", label="t"))

    assert captured[0].permission_mode == "default"


def test_allowed_tools_from_native_block(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient(
        "claude-sonnet-4-6",
        native_block={"allowed_tools": ["Read", "Edit"]},
    )
    asyncio.run(client.run("hello", label="t"))

    opts = captured[0]
    assert hasattr(opts, "allowed_tools")
    assert opts.allowed_tools == ["Read", "Edit"]


def test_no_allowed_tools_when_not_in_native_block(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6")
    asyncio.run(client.run("hello", label="t"))

    opts = captured[0]
    assert getattr(opts, "allowed_tools", None) is None


def test_no_mcp_servers_no_hooks(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "valid-key")

    captured: list[Any] = []
    mock_sdk.query = _capturing_query(captured)

    client = AnthropicSdkClient("claude-sonnet-4-6")
    asyncio.run(client.run("hello", label="t"))

    assert captured[0].mcp_servers == {}
    assert captured[0].hooks is None


# ---------------------------------------------------------------------------
# Cost computation tests
# ---------------------------------------------------------------------------


def _result_msg(**kwargs: Any) -> ResultMessage:
    defaults: dict[str, Any] = dict(
        subtype="success",
        duration_ms=10,
        duration_api_ms=8,
        is_error=False,
        num_turns=1,
        session_id="s",
        result="ok",
    )
    defaults.update(kwargs)
    return ResultMessage(**defaults)


def test_cost_known_model_from_tokens(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    # claude-sonnet-4-6: $3.0/1M input, $15.0/1M output
    # 1M input + 1M output = $3.0 + $15.0 = $18.0
    async def _query(*, prompt: Any, options: Any):
        yield _result_msg(input_tokens=1_000_000, output_tokens=1_000_000)

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result = asyncio.run(client.run("test", label="cost"))
    assert result.cost_usd == pytest.approx(18.0)


def test_cost_known_model_partial_tokens(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    # 500k input + 100k output at sonnet rates = $1.5 + $1.5 = $3.0
    async def _query(*, prompt: Any, options: Any):
        yield _result_msg(input_tokens=500_000, output_tokens=100_000)

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result = asyncio.run(client.run("test", label="cost"))
    assert result.cost_usd == pytest.approx(3.0)


def test_cost_unknown_model_falls_back_to_sonnet(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    # Unknown model falls back to sonnet rates (3.0/1M input, 15.0/1M output)
    # 1M input + 1M output = $18.0
    async def _query(*, prompt: Any, options: Any):
        yield _result_msg(input_tokens=1_000_000, output_tokens=1_000_000)

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-unknown-99")
    result = asyncio.run(client.run("test", label="cost"))
    assert result.cost_usd == pytest.approx(18.0)


def test_cost_falls_back_to_total_cost_usd(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    # No token counts → fall back to SDK-reported total_cost_usd
    async def _query(*, prompt: Any, options: Any):
        yield _result_msg(total_cost_usd=0.042)

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result = asyncio.run(client.run("test", label="cost"))
    assert result.cost_usd == pytest.approx(0.042)


def test_cost_none_when_no_data(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    async def _query(*, prompt: Any, options: Any):
        yield _result_msg(total_cost_usd=None)

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result = asyncio.run(client.run("test", label="cost"))
    assert result.cost_usd is None


# ---------------------------------------------------------------------------
# Retry policy tests
# ---------------------------------------------------------------------------


def test_retry_on_transient_error(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    async def _no_sleep(s: float) -> None:
        pass

    monkeypatch.setattr(asyncio, "sleep", _no_sleep)

    call_count = [0]

    async def _query(*, prompt: Any, options: Any):
        call_count[0] += 1
        if call_count[0] == 1:
            raise RuntimeError("rate limit exceeded — please retry")
        yield _result_msg()

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result = asyncio.run(client.run("test", label="retry", max_retries=2))
    assert result.exit_code == 0
    assert call_count[0] == 2


def test_no_retry_on_non_transient_error(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    call_count = [0]

    async def _query(*, prompt: Any, options: Any):
        call_count[0] += 1
        raise RuntimeError("authentication failed — invalid api key")
        yield  # make it a generator

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    with pytest.raises(RuntimeError, match="authentication failed"):
        asyncio.run(client.run("test", label="no-retry", max_retries=2))
    assert call_count[0] == 1


def test_retry_exhaustion_raises(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    async def _no_sleep(s: float) -> None:
        pass

    monkeypatch.setattr(asyncio, "sleep", _no_sleep)

    async def _query(*, prompt: Any, options: Any):
        raise RuntimeError("overloaded")
        yield

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    with pytest.raises(StreamTerminalError):
        asyncio.run(client.run("test", label="exhaust", max_retries=2))


def test_on_timeout_prompt_switches_on_retry(monkeypatch, mock_sdk):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "key")

    async def _no_sleep(s: float) -> None:
        pass

    monkeypatch.setattr(asyncio, "sleep", _no_sleep)

    prompts_seen: list[str] = []

    async def _query(*, prompt: Any, options: Any):
        prompts_seen.append(prompt)
        if len(prompts_seen) == 1:
            # Simulate idle timeout on first attempt
            raise StreamTimeoutError("anthropic SDK stream idle timeout")
        yield _result_msg()

    mock_sdk.query = _query
    client = AnthropicSdkClient("claude-sonnet-4-6")
    result = asyncio.run(
        client.run(
            "original prompt",
            label="timeout",
            max_retries=2,
            on_timeout_prompt="resume prompt",
        )
    )
    assert result.exit_code == 0
    assert prompts_seen[0] == "original prompt"
    assert prompts_seen[1] == "resume prompt"
