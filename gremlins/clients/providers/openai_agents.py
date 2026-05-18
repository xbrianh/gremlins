from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys
import threading
from typing import Any

from agents import Agent, ModelSettings, RunConfig, Runner, Usage
from agents.items import (
    MessageOutputItem,
    ReasoningItem,
    ToolCallItem,
    ToolCallOutputItem,
)
from agents.models.openai_provider import OpenAIProvider
from agents.result import RunResultStreaming
from agents.stream_events import RunItemStreamEvent
from openai.types.shared import Reasoning

from gremlins.clients.config import (
    OPENAI_AGENTS_MAX_TURNS,
    STREAM_IDLE_BACKOFF,
    STREAM_IDLE_TIMEOUT,
    is_transient_stream_error,
    retry,
    validate_max_retries,
)
from gremlins.clients.protocol import CompletedRun
from gremlins.clients.stream import trunc
from gremlins.clients.tools import build_tools
from gremlins.utils.decorators import default_on_exception, swallow
from gremlins.utils.yaml_io import load_bundled_prompt

# USD per 1M tokens: (input, output)
_PRICING: dict[str, tuple[float, float]] = {
    "gpt-4o": (2.50, 10.00),
    "gpt-4o-mini": (0.15, 0.60),
    "gpt-4o-2024-11-20": (2.50, 10.00),
    "gpt-4o-2024-08-06": (2.50, 10.00),
    "gpt-4.1": (2.00, 8.00),
    "gpt-4.1-mini": (0.40, 1.60),
    "gpt-4.1-nano": (0.10, 0.40),
    "gpt-4-turbo": (10.00, 30.00),
    # o1/o3: Usage.output_tokens bundles reasoning tokens; prices cover blended rate.
    "o1": (15.00, 60.00),
    "o1-mini": (1.10, 4.40),
    "o3": (10.00, 40.00),
    "o3-mini": (1.10, 4.40),
    "o4-mini": (1.10, 4.40),
    # xAI Grok models
    "grok-3": (3.00, 15.00),
    "grok-3-fast": (0.60, 4.00),
    "grok-3-mini": (0.30, 0.50),
    "grok-3-mini-fast": (0.06, 0.40),
    "grok-4": (3.00, 15.00),
}
_DEFAULT_PRICING = (2.50, 10.00)
_DEFAULT_TEMPERATURE = 0.3

DEFAULT_INSTRUCTIONS = load_bundled_prompt("default_openai_agents_instructions.md")


class StreamTimeoutError(RuntimeError):
    pass


class StreamTerminalError(RuntimeError):
    pass


def _compute_cost(model: str, usage: Usage) -> float:
    input_price, output_price = _PRICING.get(model, _DEFAULT_PRICING)
    return (
        usage.input_tokens * input_price + usage.output_tokens * output_price
    ) / 1_000_000


@default_on_exception({})
def _parse_args_json(args_json: str) -> dict[str, Any]:
    return json.loads(args_json)


def _key_arg(args_json: str) -> str:
    inp = _parse_args_json(args_json)
    for k in ("file_path", "command", "pattern", "url", "output_file"):
        if inp.get(k):
            return str(inp[k])
    return ""


@default_on_exception(0.0)
def _compute_run_cost(model: str, run: RunResultStreaming) -> float:
    return _compute_cost(model, run.context_wrapper.usage)


def _message_text(item: MessageOutputItem) -> str:
    content = getattr(item.raw_item, "content", []) or []
    parts: list[str] = []
    for c in content:
        txt = getattr(c, "text", None)
        if txt:
            parts.append(str(txt))
    return " ".join(parts)


def _reasoning_text(item: ReasoningItem) -> str:
    summary = getattr(item.raw_item, "summary", []) or []
    parts: list[str] = []
    for s in summary:
        txt = getattr(s, "text", None)
        if txt:
            parts.append(str(txt))
    return " ".join(parts)


def _raw_dict(event: Any) -> dict[str, Any]:
    d: dict[str, Any] = {"type": getattr(event, "type", "unknown")}
    item = getattr(event, "item", None)
    if item is not None:
        d["item_type"] = getattr(item, "type", None)
        raw_item = getattr(item, "raw_item", None)
        if raw_item is not None:
            if hasattr(raw_item, "model_dump"):
                try:
                    d["raw_item"] = raw_item.model_dump()
                except Exception:
                    d["raw_item"] = str(raw_item)
            else:
                d["raw_item"] = str(raw_item)
    return d


class OpenAIAgentsClient:
    def __init__(
        self,
        model: str | None,
        *,
        base_url: str | None = None,
        api_key: str | None = None,
        model_settings: ModelSettings | None = None,
        instructions: str = DEFAULT_INSTRUCTIONS,
    ) -> None:
        self._model = model or "gpt-4o"
        self._total_cost_usd = 0.0
        self._base_url = base_url
        self._api_key = api_key
        self._model_settings = model_settings
        self._instructions = instructions
        self._provider: OpenAIProvider | None = (
            OpenAIProvider(base_url=base_url, api_key=api_key)
            if base_url or api_key
            else None
        )
        # RLock so signal handlers on the main thread don't deadlock
        self._lock = threading.RLock()
        self._active_runs: list[RunResultStreaming] = []

    def _track(self, run: RunResultStreaming) -> None:
        with self._lock:
            self._active_runs.append(run)

    @swallow(ValueError)
    def _untrack(self, run: RunResultStreaming) -> None:
        with self._lock:
            self._active_runs.remove(run)

    def reap_all(self) -> None:
        with self._lock:
            runs = list(self._active_runs)
        for run in runs:
            try:
                run.cancel()
            except Exception:
                pass

    def run(
        self,
        prompt: str,
        *,
        label: str,
        model: str | None = None,
        raw_path: pathlib.Path | None = None,
        capture_events: bool = False,
        on_timeout_prompt: str | None = None,
        max_retries: int = 2,
        cwd: pathlib.Path | None = None,
        idle_timeout: float | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> CompletedRun:
        validate_max_retries(max_retries)
        if idle_timeout is None:
            idle_timeout = STREAM_IDLE_TIMEOUT
        effective_model = model or self._model
        prefix = f"[{label}] " if label else ""
        agent = Agent(
            name=f"gremlins-{label}",
            instructions=self._instructions,
            tools=GREMLINS_TOOLS,
            model=effective_model,
            model_settings=self._model_settings
            if self._model_settings is not None
            else ModelSettings(),
        )
        ctx: dict[str, object] = {
            "cwd": str(cwd) if cwd is not None else None,
            "extra_env": extra_env,
        }
        run_config = (
            RunConfig(tracing_disabled=True, model_provider=self._provider)
            if self._provider is not None
            else RunConfig(tracing_disabled=True)
        )
        active_prompt = prompt

        def _on_retry(attempt: int, exc: BaseException, wait: float) -> None:
            nonlocal active_prompt
            label_str = (
                "idle timeout"
                if isinstance(exc, StreamTimeoutError)
                else "transient-error"
            )
            sys.stderr.write(
                f"{prefix}stream {label_str}, retrying in {wait}s"
                f" ({attempt + 1}/{max_retries})...\n"
            )
            if isinstance(exc, StreamTimeoutError) and on_timeout_prompt is not None:
                active_prompt = on_timeout_prompt

        def _should_retry(exc: BaseException) -> bool:
            return isinstance(exc, StreamTimeoutError) or is_transient_stream_error(
                str(exc)
            )

        @retry(
            StreamTimeoutError,
            StreamTerminalError,
            backoff=STREAM_IDLE_BACKOFF[:max_retries],
            classify=_should_retry,
            on_retry=_on_retry,
        )
        def _run_once() -> CompletedRun:
            return asyncio.run(
                self._run_streamed(
                    agent,
                    active_prompt,
                    ctx,
                    run_config,
                    prefix=prefix,
                    model=effective_model,
                    raw_path=raw_path,
                    capture_events=capture_events,
                    cwd=cwd,
                    idle_timeout=idle_timeout,
                )
            )

        try:
            return _run_once()
        except StreamTerminalError as exc:
            if is_transient_stream_error(str(exc)):
                sys.stderr.write(
                    f"{prefix}stream transient-error, retries exhausted, failing\n"
                )
            else:
                sys.stderr.write(f"{prefix}stream permanent-error, failing\n")
            raise

    async def _run_streamed(
        self,
        agent: Agent,
        prompt: str,
        ctx: dict[str, object],
        run_config: RunConfig,
        *,
        prefix: str,
        model: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
        cwd: pathlib.Path | None,
        idle_timeout: float,
    ) -> CompletedRun:
        run = Runner.run_streamed(
            agent,
            prompt,
            context=ctx,
            run_config=run_config,
            max_turns=OPENAI_AGENTS_MAX_TURNS,
        )
        self._track(run)

        sys.stderr.write(f"{prefix}init model={model} cwd={str(cwd) if cwd else '?'}\n")
        sys.stderr.flush()

        raw = open(raw_path, "ab") if raw_path is not None else None
        captured: list[dict[str, Any]] | None = [] if capture_events else None
        turns = 0
        timed_out = False
        stream_error: list[Exception] = []

        event_queue: asyncio.Queue[Any] = asyncio.Queue()

        async def _stream_to_queue() -> None:
            try:
                async for event in run.stream_events():
                    await event_queue.put(event)
            except Exception as exc:
                sys.stderr.write(f"{prefix}stream error: {exc}\n")
                stream_error.append(exc)
                try:
                    run.cancel()
                except Exception:
                    pass
            finally:
                await event_queue.put(None)

        stream_task = asyncio.create_task(_stream_to_queue())

        try:
            while True:
                try:
                    event = await asyncio.wait_for(
                        event_queue.get(), timeout=idle_timeout
                    )
                except TimeoutError:
                    timed_out = True
                    run.cancel()
                    stream_task.cancel()
                    try:
                        await stream_task
                    except (asyncio.CancelledError, Exception):
                        pass
                    break

                if event is None:
                    break

                if raw is not None:
                    try:
                        raw.write((json.dumps(_raw_dict(event)) + "\n").encode())
                        raw.flush()
                    except Exception:
                        pass

                if not isinstance(event, RunItemStreamEvent):
                    continue

                item = event.item

                if isinstance(item, MessageOutputItem):
                    text = _message_text(item)
                    sys.stderr.write(f"{prefix}text: {trunc(text)}\n")
                    if captured is not None:
                        captured.append(
                            {
                                "type": "assistant",
                                "message": {
                                    "content": [{"type": "text", "text": text}]
                                },
                            }
                        )

                elif isinstance(item, ReasoningItem):
                    sys.stderr.write(f"{prefix}think: {trunc(_reasoning_text(item))}\n")

                elif isinstance(item, ToolCallItem):
                    name = item.tool_name or "?"
                    args_json = getattr(item.raw_item, "arguments", "") or ""
                    call_id = item.call_id or ""
                    sys.stderr.write(
                        f"{prefix}tool: {name} {trunc(_key_arg(args_json))}\n"
                    )
                    if captured is not None:
                        captured.append(
                            {
                                "type": "assistant",
                                "message": {
                                    "content": [
                                        {
                                            "type": "tool_use",
                                            "id": call_id,
                                            "name": name,
                                            "input": _parse_args_json(args_json),
                                        }
                                    ]
                                },
                            }
                        )

                elif isinstance(item, ToolCallOutputItem):
                    output = str(item.output) if item.output is not None else ""
                    call_id = item.call_id or ""
                    sys.stderr.write(f"{prefix}result: {trunc(output)}\n")
                    if captured is not None:
                        captured.append(
                            {
                                "type": "user",
                                "message": {
                                    "content": [
                                        {
                                            "type": "tool_result",
                                            "tool_use_id": call_id,
                                            "content": output,
                                        }
                                    ]
                                },
                            }
                        )
                    turns += 1

                sys.stderr.flush()

        finally:
            self._untrack(run)
            if raw is not None:
                raw.close()

        cost = _compute_run_cost(model, run)
        with self._lock:
            self._total_cost_usd += cost

        if timed_out:
            suffix = " (timeout)"
        elif stream_error:
            suffix = " (stream-error)"
        else:
            suffix = ""
        sys.stderr.write(f"{prefix}final: turns={turns} cost={cost:.6f}{suffix}\n")
        sys.stderr.flush()

        if timed_out:
            raise StreamTimeoutError("openai-agents stream idle timeout")
        if stream_error:
            raise StreamTerminalError(str(stream_error[0])) from stream_error[0]

        text = str(run.final_output) if run.final_output is not None else None
        return CompletedRun(
            exit_code=0, text_result=text, events=captured, cost_usd=cost
        )

    @property
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    @property
    def base_url(self) -> str | None:
        return self._base_url

    @property
    def api_key(self) -> str | None:
        return self._api_key


def make_openai_client(model: str | None) -> OpenAIAgentsClient:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        raise RuntimeError("OPENAI_API_KEY environment variable is not set")
    return OpenAIAgentsClient(
        model,
        api_key=api_key,
        model_settings=ModelSettings(temperature=_DEFAULT_TEMPERATURE),
    )


def make_xai_client(model: str | None) -> OpenAIAgentsClient:
    api_key = os.environ.get("XAI_API_KEY")
    if not api_key:
        raise RuntimeError("XAI_API_KEY environment variable is not set")
    return OpenAIAgentsClient(
        model or "grok-4",
        base_url="https://api.x.ai/v1",
        api_key=api_key,
        model_settings=ModelSettings(
            temperature=_DEFAULT_TEMPERATURE,
            reasoning=Reasoning(effort="high", summary="auto"),
        ),
    )
