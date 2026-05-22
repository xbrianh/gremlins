from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys
from collections.abc import AsyncGenerator
from typing import Any, cast

from gremlins.clients.config import (
    STREAM_IDLE_BACKOFF,
    STREAM_IDLE_TIMEOUT,
    is_transient_stream_error,
    retry,
    validate_max_retries,
)
from gremlins.clients.protocol import CompletedRun
from gremlins.clients.stream import emit_event, ts
from gremlins.permissions.loader import load_default_block
from gremlins.permissions.policy import Policy

_DEFAULT_MODEL = "claude-sonnet-4-6"

# Pricing per 1M tokens (input, output) in USD.
_PRICING: dict[str, tuple[float, float]] = {
    "claude-opus-4-7": (15.0, 75.0),
    "claude-opus-4-6": (15.0, 75.0),
    "claude-sonnet-4-7": (3.0, 15.0),
    "claude-sonnet-4-6": (3.0, 15.0),
    "claude-sonnet-4-5": (3.0, 15.0),
    "claude-haiku-4-5": (0.8, 4.0),
    "claude-haiku-4-5-20251001": (0.8, 4.0),
}
_DEFAULT_PRICING = _PRICING["claude-sonnet-4-6"]


class StreamTimeoutError(RuntimeError):
    pass


class StreamTerminalError(RuntimeError):
    pass


def _cost_usd(model: str, input_tokens: int, output_tokens: int) -> float:
    input_rate, output_rate = _PRICING.get(model, _DEFAULT_PRICING)
    return (input_tokens * input_rate + output_tokens * output_rate) / 1_000_000


def _scrub_env(extra_env: dict[str, str] | None) -> dict[str, str]:
    env: dict[str, str] = {}
    for k, v in os.environ.items():
        if k.startswith("CLAUDE_"):
            continue
        if k.startswith("ANTHROPIC_") and k != "ANTHROPIC_API_KEY":
            continue
        env[k] = v
    if extra_env:
        env.update(extra_env)
    return env


def _block_to_dict(block: Any) -> dict[str, Any] | None:
    cls = type(block).__name__
    if cls == "TextBlock":
        return {"type": "text", "text": block.text}
    if cls == "ThinkingBlock":
        return {"type": "thinking", "thinking": block.thinking}
    if cls == "ToolUseBlock":
        return {
            "type": "tool_use",
            "id": block.id,
            "name": block.name,
            "input": block.input,
        }
    if cls == "ToolResultBlock":
        return {
            "type": "tool_result",
            "tool_use_id": block.tool_use_id,
            "content": block.content,
            "is_error": block.is_error,
        }
    return None


def _extract_content(blocks: Any) -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    items: list[Any] = blocks if isinstance(blocks, list) else []  # type: ignore[assignment]
    for b in items:
        d = _block_to_dict(b)
        if d is not None:
            result.append(d)
    return result


def _msg_to_event(msg: Any) -> dict[str, Any] | None:
    cls = type(msg).__name__
    if cls == "SystemMessage":
        return {"type": "system", "subtype": msg.subtype, **msg.data}
    if cls == "AssistantMessage":
        return {
            "type": "assistant",
            "message": {"role": "assistant", "content": _extract_content(msg.content)},
        }
    if cls == "UserMessage":
        raw: Any = msg.content
        if not isinstance(raw, list):
            return {
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": str(raw)}],
                },
            }
        return {
            "type": "user",
            "message": {"role": "user", "content": _extract_content(raw)},
        }
    if cls == "ResultMessage":
        return {
            "type": "result",
            "subtype": msg.subtype,
            "num_turns": msg.num_turns,
            "total_cost_usd": msg.total_cost_usd,
            "result": msg.result,
            "is_error": msg.is_error,
        }
    return None


def _extract_cost(msg: Any, model: str) -> float | None:
    input_tok = getattr(msg, "input_tokens", None)
    output_tok = getattr(msg, "output_tokens", None)
    if isinstance(input_tok, int) and isinstance(output_tok, int):
        return _cost_usd(model, input_tok, output_tok)
    raw = getattr(msg, "total_cost_usd", None)
    return float(raw) if isinstance(raw, (int, float)) else None


class AnthropicSdkClient:
    def __init__(
        self,
        model: str | None,
        bypass: bool = False,
        native_block: dict[str, Any] | None = None,
    ) -> None:
        api_key = os.environ.get("ANTHROPIC_API_KEY")
        if not api_key:
            raise RuntimeError("ANTHROPIC_API_KEY environment variable is not set")
        self._model = model or _DEFAULT_MODEL
        self._bypass = bypass
        self._native_block: dict[str, Any] = (
            native_block if native_block is not None else {}
        )

    async def _execute(
        self,
        prompt: str,
        *,
        effective_model: str,
        prefix: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
        cwd: pathlib.Path | None,
        idle_timeout: float,
        extra_env: dict[str, str] | None,
    ) -> CompletedRun:
        import claude_agent_sdk  # type: ignore[import-untyped]

        permission_mode = "bypassPermissions" if self._bypass else "default"
        opts_kwargs: dict[str, Any] = {
            "model": effective_model,
            "cwd": cwd,
            "permission_mode": permission_mode,
            "setting_sources": [],
            "mcp_servers": {},
            "hooks": None,
            "env": _scrub_env(extra_env),
        }
        allowed_tools: list[str] | None = self._native_block.get("allowed_tools")
        if allowed_tools is not None:
            opts_kwargs["allowed_tools"] = allowed_tools
        disallowed_tools: list[str] | None = self._native_block.get("disallowed_tools")
        if disallowed_tools is not None:
            opts_kwargs["disallowed_tools"] = disallowed_tools
        options: Any = claude_agent_sdk.ClaudeAgentOptions(  # pyright: ignore[reportUnknownMemberType, reportUnknownVariableType]
            **opts_kwargs
        )

        sys.stderr.write(
            f"{ts()} {prefix}init model={effective_model} cwd={str(cwd) if cwd else '?'}\n"
        )
        sys.stderr.flush()

        raw = open(raw_path, "ab") if raw_path is not None else None
        captured: list[dict[str, Any]] | None = [] if capture_events else None
        result_evt: dict[str, Any] | None = None
        cost: float | None = None

        gen: AsyncGenerator[Any, None] | None = None
        try:
            gen = cast(
                AsyncGenerator[Any, None],
                claude_agent_sdk.query(prompt=prompt, options=options),  # pyright: ignore[reportUnknownMemberType]
            )
            while True:
                try:
                    msg = await asyncio.wait_for(gen.__anext__(), timeout=idle_timeout)
                except StopAsyncIteration:
                    break
                except TimeoutError:
                    raise StreamTimeoutError("anthropic SDK stream idle timeout")
                except (StreamTimeoutError, StreamTerminalError):
                    raise
                except Exception as exc:
                    if is_transient_stream_error(str(exc)):
                        raise StreamTerminalError(str(exc)) from exc
                    raise

                evt = _msg_to_event(msg)
                if evt is None:
                    continue
                if raw is not None:
                    try:
                        raw.write((json.dumps(evt) + "\n").encode())
                        raw.flush()
                    except Exception:
                        pass
                if captured is not None:
                    captured.append(evt)
                emit_event(prefix, evt)
                if type(msg).__name__ == "ResultMessage":
                    result_evt = evt
                    cost = _extract_cost(msg, effective_model)
        finally:
            if raw is not None:
                raw.close()
            if gen is not None:
                try:
                    await gen.aclose()
                except Exception:
                    pass

        exit_code = 1 if result_evt is None or result_evt.get("is_error") else 0
        text_result = result_evt.get("result") if result_evt is not None else None
        return CompletedRun(
            exit_code=exit_code,
            text_result=text_result,
            events=captured,
            cost_usd=cost,
        )

    async def run(
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
        active_prompt = prompt

        def _on_retry(attempt: int, exc: BaseException, wait: float) -> None:
            nonlocal active_prompt
            label_str = (
                "idle timeout"
                if isinstance(exc, StreamTimeoutError)
                else "transient-error"
            )
            sys.stderr.write(
                f"{ts()} {prefix}stream {label_str}, retrying in {wait}s"
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
        async def _run_once() -> CompletedRun:
            return await self._execute(
                active_prompt,
                effective_model=effective_model,
                prefix=prefix,
                raw_path=raw_path,
                capture_events=capture_events,
                cwd=cwd,
                idle_timeout=idle_timeout,
                extra_env=extra_env,
            )

        try:
            return await _run_once()
        except StreamTerminalError:
            sys.stderr.write(
                f"{ts()} {prefix}stream transient-error, retries exhausted, failing\n"
            )
            raise

    def reap_all(self) -> None:
        pass

    @property
    def total_cost_usd(self) -> float | None:
        return None


def make_anthropic_client(model: str | None, policy: Policy) -> AnthropicSdkClient:
    return AnthropicSdkClient(
        model,
        bypass=policy.bypass,
        native_block=load_default_block("anthropic") | policy.block_for("anthropic"),
    )
