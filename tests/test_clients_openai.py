"""Tests for OpenAIAgentsClient and GREMLINS_TOOLS."""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from agents import ModelSettings, Usage
from agents.items import MessageOutputItem, ToolCallItem, ToolCallOutputItem
from agents.stream_events import RunItemStreamEvent
from agents.tool_context import ToolContext

from gremlins.clients.config import OPENAI_AGENTS_MAX_TURNS
from gremlins.clients.protocol import CompletedRun
from gremlins.clients.providers.openai_agents import (
    DEFAULT_INSTRUCTIONS,
    OpenAIAgentsClient,
    StreamTerminalError,
    StreamTimeoutError,
    _key_arg,
    make_openai_client,
    make_xai_client,
)
from gremlins.clients.tools import GREMLINS_TOOLS, _bash_invoke, build_tools
from gremlins.permissions.policy import Policy


def test_openai_client_constructs() -> None:
    client = OpenAIAgentsClient("gpt-4o")
    assert client.total_cost_usd is None


def test_default_instructions_are_substantive() -> None:
    assert "re-check" in DEFAULT_INSTRUCTIONS
    assert "bail marker" in DEFAULT_INSTRUCTIONS
    assert "audit" in DEFAULT_INSTRUCTIONS


def test_custom_instructions_passed_to_agent(monkeypatch: Any) -> None:
    usage = _make_usage()
    fake_run = _make_run_result_streaming("done", usage, [])
    captured_agents: list[Any] = []

    def _fake_run_streamed(agent: Any, *a: Any, **kw: Any) -> Any:
        captured_agents.append(agent)
        return fake_run

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    custom = "Custom instructions here."
    client = OpenAIAgentsClient("gpt-4o", instructions=custom)
    asyncio.run(client.run("do something", label="t"))

    assert captured_agents, "Runner.run_streamed was not called"
    assert captured_agents[0].instructions == custom


def test_gremlins_tools_nonempty() -> None:
    assert len(GREMLINS_TOOLS) > 0
    for tool in GREMLINS_TOOLS:
        assert hasattr(tool, "name") and tool.name
        assert hasattr(tool, "params_json_schema") and isinstance(
            tool.params_json_schema, dict
        )


def _make_usage(input_tokens: int = 100, output_tokens: int = 50) -> Usage:
    return Usage(
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        total_tokens=input_tokens + output_tokens,
        requests=1,
    )


def _make_run_result_streaming(
    final_output: str,
    usage: Usage,
    events: list[Any],
) -> MagicMock:
    """Build a fake RunResultStreaming that yields a sequence of stream events."""

    async def _fake_stream_events() -> Any:
        for ev in events:
            yield ev

    ctx_wrapper = MagicMock()
    ctx_wrapper.usage = usage

    run = MagicMock()
    run.stream_events.return_value = _fake_stream_events()
    run.final_output = final_output
    run.context_wrapper = ctx_wrapper
    run.cancel = MagicMock()
    return run


def _make_run_item_event(item: Any) -> RunItemStreamEvent:
    ev = MagicMock(spec=RunItemStreamEvent)
    ev.type = "run_item_stream_event"
    ev.item = item
    return ev


def _make_message_item(text: str) -> MessageOutputItem:
    raw = MagicMock()
    raw.content = [MagicMock(text=text)]
    raw.model_dump = MagicMock(return_value={"type": "message", "content": text})
    item = MagicMock(spec=MessageOutputItem)
    item.type = "message_output_item"
    item.raw_item = raw
    return item


def _make_tool_call_item(
    name: str, args: dict[str, Any], call_id: str = "call_1"
) -> ToolCallItem:
    raw = MagicMock()
    raw.arguments = json.dumps(args)
    raw.name = name
    raw.model_dump = MagicMock(return_value={"type": "function_call", "name": name})
    item = MagicMock(spec=ToolCallItem)
    item.type = "tool_call_item"
    item.raw_item = raw
    item.tool_name = name
    item.call_id = call_id
    return item


def _make_tool_output_item(output: str, call_id: str = "call_1") -> ToolCallOutputItem:
    raw = MagicMock()
    raw.model_dump = MagicMock(return_value={"type": "function_call_output"})
    item = MagicMock(spec=ToolCallOutputItem)
    item.type = "tool_call_output_item"
    item.raw_item = raw
    item.output = output
    item.call_id = call_id
    return item


def test_key_arg_picks_file_path() -> None:
    assert _key_arg(json.dumps({"file_path": "/foo/bar.py"})) == "/foo/bar.py"


def test_key_arg_picks_command() -> None:
    assert _key_arg(json.dumps({"command": "echo hi"})) == "echo hi"


def test_key_arg_empty_on_bad_json() -> None:
    assert _key_arg("not-json") == ""


def test_streamed_run_produces_completed_run(monkeypatch: Any) -> None:
    usage = _make_usage(100, 50)
    msg_item = _make_message_item("Task done.")
    tool_item = _make_tool_call_item("Read", {"file_path": "/tmp/x.py"}, "c1")
    out_item = _make_tool_output_item("file contents", "c1")

    stream_events = [
        _make_run_item_event(msg_item),
        _make_run_item_event(tool_item),
        _make_run_item_event(out_item),
    ]
    fake_run = _make_run_result_streaming("Task done.", usage, stream_events)

    monkeypatch.setattr("agents.run.Runner.run_streamed", lambda *a, **kw: fake_run)

    client = OpenAIAgentsClient("gpt-4o")
    result = asyncio.run(client.run("do something", label="test"))

    assert isinstance(result, CompletedRun)
    assert result.exit_code == 0
    assert result.text_result == "Task done."
    assert result.cost_usd is None
    assert client.total_cost_usd is None


def test_streamed_run_log_lines(
    monkeypatch: Any, capsys: Any, tmp_path: pathlib.Path
) -> None:
    usage = _make_usage(10, 10)
    tool_item = _make_tool_call_item("Bash", {"command": "ls /tmp"}, "c2")
    out_item = _make_tool_output_item("a.py\nb.py", "c2")

    fake_run = _make_run_result_streaming(
        "done", usage, [_make_run_item_event(tool_item), _make_run_item_event(out_item)]
    )
    monkeypatch.setattr("agents.run.Runner.run_streamed", lambda *a, **kw: fake_run)

    client = OpenAIAgentsClient("gpt-4o")
    asyncio.run(client.run("ls", label="mylabel", cwd=tmp_path))

    err = capsys.readouterr().err
    assert f"[mylabel] init model=gpt-4o cwd={tmp_path}" in err
    assert "[mylabel] tool: Bash ls /tmp" in err
    assert "[mylabel] result: a.py b.py" in err
    assert "[mylabel] final: turns=1 cost=" in err


def test_streamed_run_raw_path(monkeypatch: Any, tmp_path: pathlib.Path) -> None:
    usage = _make_usage(10, 5)
    tool_item = _make_tool_call_item("Read", {"file_path": "/a.py"}, "c3")
    out_item = _make_tool_output_item("content", "c3")

    fake_run = _make_run_result_streaming(
        "done", usage, [_make_run_item_event(tool_item), _make_run_item_event(out_item)]
    )
    monkeypatch.setattr("agents.run.Runner.run_streamed", lambda *a, **kw: fake_run)

    raw_path = tmp_path / "stream.jsonl"
    client = OpenAIAgentsClient("gpt-4o")
    asyncio.run(client.run("read", label="t", raw_path=raw_path))

    lines = raw_path.read_text().splitlines()
    assert len(lines) >= 2
    for line in lines:
        obj = json.loads(line)
        assert "type" in obj


def test_capture_events_tool_call_shape(monkeypatch: Any) -> None:
    usage = _make_usage(10, 5)
    tool_item = _make_tool_call_item("Bash", {"command": "gh pr create"}, "id42")
    out_item = _make_tool_output_item("https://github.com/org/repo/pull/7", "id42")

    fake_run = _make_run_result_streaming(
        "done", usage, [_make_run_item_event(tool_item), _make_run_item_event(out_item)]
    )
    monkeypatch.setattr("agents.run.Runner.run_streamed", lambda *a, **kw: fake_run)

    client = OpenAIAgentsClient("gpt-4o")
    result = asyncio.run(client.run("create pr", label="t", capture_events=True))

    assert result.events is not None
    tool_evts = [
        e
        for e in result.events
        if e.get("type") == "assistant"
        and any(
            c.get("type") == "tool_use" for c in e.get("message", {}).get("content", [])
        )
    ]
    result_evts = [
        e
        for e in result.events
        if e.get("type") == "user"
        and any(
            c.get("type") == "tool_result"
            for c in e.get("message", {}).get("content", [])
        )
    ]
    assert len(tool_evts) == 1
    assert len(result_evts) == 1

    tool_content = tool_evts[0]["message"]["content"][0]
    assert tool_content["name"] == "Bash"
    assert tool_content["id"] == "id42"
    assert tool_content["input"]["command"] == "gh pr create"

    result_content = result_evts[0]["message"]["content"][0]
    assert result_content["tool_use_id"] == "id42"
    assert "github.com" in result_content["content"]


def test_idle_timeout_calls_run_cancel(monkeypatch: Any) -> None:
    usage = _make_usage()
    cancel_called: list[bool] = []

    async def _blocking_stream() -> Any:
        await asyncio.sleep(9999)
        yield MagicMock()  # never reached; makes this an async generator

    run = MagicMock()
    run.stream_events.return_value = _blocking_stream()
    run.final_output = None
    run.context_wrapper = MagicMock()
    run.context_wrapper.usage = usage
    run.cancel = lambda: cancel_called.append(True)

    monkeypatch.setattr("agents.run.Runner.run_streamed", lambda *a, **kw: run)

    client = OpenAIAgentsClient("gpt-4o")
    with pytest.raises(StreamTimeoutError):
        asyncio.run(client.run("block", label="t", idle_timeout=0.05, max_retries=0))

    assert cancel_called


def test_reap_all_calls_cancel_on_tracked_runs() -> None:
    client = OpenAIAgentsClient("gpt-4o")
    fake_run = MagicMock()
    fake_run.cancel = MagicMock()
    client._active_runs.append(fake_run)
    client.reap_all()
    fake_run.cancel.assert_called_once()


def test_idle_timeout_raises_stream_timeout_error(monkeypatch: Any) -> None:
    async def _slow_stream() -> Any:
        await asyncio.sleep(9999)
        yield MagicMock()

    run = MagicMock()
    run.stream_events.return_value = _slow_stream()
    run.cancel = MagicMock()
    run.final_output = None
    run.context_wrapper = MagicMock()
    run.context_wrapper.usage = _make_usage()

    monkeypatch.setattr("agents.run.Runner.run_streamed", lambda *a, **kw: run)

    client = OpenAIAgentsClient("gpt-4o")
    with pytest.raises(StreamTimeoutError):
        asyncio.run(client.run("slow", label="t", idle_timeout=0.05, max_retries=0))


def test_model_settings_stored_and_passed_to_agent(monkeypatch: Any) -> None:
    usage = _make_usage()
    fake_run = _make_run_result_streaming("done", usage, [])
    captured_agents: list[Any] = []

    def _fake_run_streamed(agent: Any, *a: Any, **kw: Any) -> Any:
        captured_agents.append(agent)
        return fake_run

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    settings = ModelSettings(temperature=0.5)
    client = OpenAIAgentsClient("gpt-4o", model_settings=settings)
    assert client._model_settings is settings
    asyncio.run(client.run("do something", label="t"))

    assert captured_agents, "Runner.run_streamed was not called"
    assert captured_agents[0].model_settings.temperature == 0.5


def test_xai_client_constructs(monkeypatch: Any) -> None:
    monkeypatch.setenv("XAI_API_KEY", "test-key")
    client = make_xai_client(None, Policy())
    assert isinstance(client, OpenAIAgentsClient)
    assert client._model == "grok-4"
    assert client.base_url == "https://api.x.ai/v1"
    assert client.api_key == "test-key"


def test_xai_client_model_settings(monkeypatch: Any) -> None:
    monkeypatch.setenv("XAI_API_KEY", "test-key")
    client = make_xai_client(None, Policy())
    assert client._model_settings is not None
    assert client._model_settings.temperature == 0.3
    assert client._model_settings.reasoning is not None
    assert client._model_settings.reasoning.effort == "high"
    assert client._model_settings.reasoning.summary == "auto"


def test_xai_client_missing_key(monkeypatch: Any) -> None:
    monkeypatch.delenv("XAI_API_KEY", raising=False)
    with pytest.raises(RuntimeError):
        make_xai_client(None, Policy())


def test_openai_client_constructs_with_api_key(monkeypatch: Any) -> None:
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    client = make_openai_client(None, Policy())
    assert isinstance(client, OpenAIAgentsClient)
    assert client.api_key == "sk-test"


def test_openai_client_model_settings(monkeypatch: Any) -> None:
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    client = make_openai_client(None, Policy())
    assert client._model_settings is not None
    assert client._model_settings.temperature == 0.3


def test_openai_client_missing_key(monkeypatch: Any) -> None:
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    with pytest.raises(RuntimeError):
        make_openai_client(None, Policy())


@pytest.mark.skipif(
    not os.environ.get("OPENAI_API_KEY"),
    reason="OPENAI_API_KEY not set",
)
def test_openai_integration_run() -> None:
    client = make_openai_client("gpt-4o-mini", Policy())
    result = asyncio.run(
        client.run("Reply with the single word: done", label="integration-test")
    )
    assert result.exit_code == 0
    assert result.text_result


@pytest.mark.skipif(
    not os.environ.get("XAI_API_KEY"),
    reason="XAI_API_KEY not set",
)
def test_xai_integration_run() -> None:
    try:
        client = make_xai_client("grok-3-mini-fast", Policy())
        result = asyncio.run(
            client.run("Reply with the single word: done", label="integration-test")
        )
        assert result.exit_code == 0
        assert result.text_result
    except Exception as e:
        if "nodename nor servname" in str(e) or "Connection error" in str(e):
            pytest.skip(f"xAI API service unavailable: {e}")
        raise


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


def _make_erroring_run(error_message: str, usage: Usage) -> MagicMock:
    """Fake run whose stream raises an exception (simulating a terminal error event)."""

    async def _error_stream() -> Any:
        raise RuntimeError(error_message)
        yield  # make it an async generator

    ctx_wrapper = MagicMock()
    ctx_wrapper.usage = usage

    run = MagicMock()
    run.stream_events.return_value = _error_stream()
    run.final_output = None
    run.context_wrapper = ctx_wrapper
    run.cancel = MagicMock()
    return run


def test_terminal_stream_error_transient_retries_and_succeeds(monkeypatch: Any) -> None:
    usage = _make_usage(50, 25)
    error_msg = "The model is currently at capacity. Please try again."
    success_usage = _make_usage(100, 50)
    msg_item = _make_message_item("done")

    call_count = [0]

    def _fake_run_streamed(*a: Any, **kw: Any) -> Any:
        call_count[0] += 1
        if call_count[0] == 1:
            return _make_erroring_run(error_msg, usage)
        return _make_run_result_streaming(
            "done", success_usage, [_make_run_item_event(msg_item)]
        )

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    async def _noop_sleep(_: float) -> None:
        pass

    monkeypatch.setattr("asyncio.sleep", _noop_sleep)

    client = OpenAIAgentsClient("gpt-4o")
    result = asyncio.run(client.run("do something", label="t", max_retries=2))

    assert call_count[0] == 2
    assert result.text_result == "done"
    assert result.cost_usd is None
    assert client.total_cost_usd is None


def test_terminal_stream_error_permanent_fails_immediately(monkeypatch: Any) -> None:
    usage = _make_usage(20, 10)
    error_msg = "Invalid API key provided"

    call_count = [0]

    def _fake_run_streamed(*a: Any, **kw: Any) -> Any:
        call_count[0] += 1
        return _make_erroring_run(error_msg, usage)

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    async def _noop_sleep(_: float) -> None:
        pass

    monkeypatch.setattr("asyncio.sleep", _noop_sleep)

    client = OpenAIAgentsClient("gpt-4o")
    with pytest.raises(StreamTerminalError):
        asyncio.run(client.run("do something", label="t", max_retries=2))

    assert call_count[0] == 1


def test_terminal_stream_error_cost_is_recorded(monkeypatch: Any) -> None:
    usage = _make_usage(100, 50)
    error_msg = "The model is currently at capacity. Please try again."
    # always error so we exhaust retries
    monkeypatch.setattr(
        "agents.run.Runner.run_streamed",
        lambda *a, **kw: _make_erroring_run(error_msg, usage),
    )

    async def _noop_sleep(_: float) -> None:
        pass

    monkeypatch.setattr("asyncio.sleep", _noop_sleep)

    client = OpenAIAgentsClient("gpt-4o")
    with pytest.raises(StreamTerminalError):
        asyncio.run(client.run("do something", label="t", max_retries=1))

    assert client.total_cost_usd is None


def test_run_streamed_passes_max_turns(monkeypatch: Any) -> None:
    usage = _make_usage()
    fake_run = _make_run_result_streaming("done", usage, [])
    captured_kwargs: list[dict[str, Any]] = []

    def _fake_run_streamed(*a: Any, **kw: Any) -> Any:
        captured_kwargs.append(kw)
        return fake_run

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    client = OpenAIAgentsClient("gpt-4o")
    asyncio.run(client.run("do something", label="t"))

    assert captured_kwargs, "Runner.run_streamed was not called"
    assert captured_kwargs[0].get("max_turns") == OPENAI_AGENTS_MAX_TURNS


def test_bypass_false_enforces_path_scoping(tmp_path: pathlib.Path) -> None:
    worktree = tmp_path / "worktree"
    worktree.mkdir()
    tools = build_tools(bypass=False, worktree_root=worktree, audit_log=None)
    edit_tool = next(t for t in tools if t.name == "Edit")

    ctx = ToolContext(
        context={"cwd": str(worktree), "extra_env": None},
        tool_name="Edit",
        tool_call_id="c1",
        tool_arguments="{}",
    )
    args_json = json.dumps(
        {"file_path": "/etc/passwd", "old_string": "x", "new_string": "y"}
    )
    result = asyncio.run(edit_tool.on_invoke_tool(ctx, args_json))
    assert result.startswith("Error: path outside worktree")


def test_native_block_tools_allowlist_filters_agent_tools(monkeypatch: Any) -> None:
    usage = _make_usage()
    fake_run = _make_run_result_streaming("done", usage, [])
    captured_agents: list[Any] = []

    def _fake_run_streamed(agent: Any, *a: Any, **kw: Any) -> Any:
        captured_agents.append(agent)
        return fake_run

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    client = OpenAIAgentsClient(
        "gpt-4o", native_block={"allowed_tools": ["Read", "Bash"]}
    )
    asyncio.run(client.run("do something", label="t"))

    assert captured_agents, "Runner.run_streamed was not called"
    tool_names = {t.name for t in captured_agents[0].tools}
    assert tool_names == {"Read", "Bash"}


def test_native_block_absent_tools_uses_all(monkeypatch: Any) -> None:
    usage = _make_usage()
    fake_run = _make_run_result_streaming("done", usage, [])
    captured_agents: list[Any] = []

    def _fake_run_streamed(agent: Any, *a: Any, **kw: Any) -> Any:
        captured_agents.append(agent)
        return fake_run

    monkeypatch.setattr("agents.run.Runner.run_streamed", _fake_run_streamed)

    client = OpenAIAgentsClient("gpt-4o")
    asyncio.run(client.run("do something", label="t"))

    assert captured_agents
    expected_tools = build_tools(
        bypass=True, worktree_root=pathlib.Path.cwd(), audit_log=None
    )
    assert len(captured_agents[0].tools) == len(expected_tools)


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
