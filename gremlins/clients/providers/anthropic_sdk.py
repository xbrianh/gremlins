from __future__ import annotations

import json
import os
import pathlib
import sys
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.clients.stream import emit_event, ts

_DEFAULT_MODEL = "claude-sonnet-4-6"


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


class AnthropicSdkClient:
    def __init__(self, model: str | None) -> None:
        api_key = os.environ.get("ANTHROPIC_API_KEY")
        if not api_key:
            raise RuntimeError("ANTHROPIC_API_KEY environment variable is not set")
        self._model = model or _DEFAULT_MODEL
        self._api_key = api_key

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
        import claude_agent_sdk

        effective_model = model or self._model
        prefix = f"[{label}] " if label else ""

        options = claude_agent_sdk.ClaudeAgentOptions(
            model=effective_model,
            cwd=cwd,
            permission_mode="bypassPermissions",
            setting_sources=[],
            mcp_servers={},
            hooks=None,
            env=_scrub_env(extra_env),
        )

        sys.stderr.write(
            f"{ts()} {prefix}init model={effective_model} cwd={str(cwd) if cwd else '?'}\n"
        )
        sys.stderr.flush()

        raw = open(raw_path, "ab") if raw_path is not None else None
        captured: list[dict[str, Any]] | None = [] if capture_events else None
        result_evt: dict[str, Any] | None = None

        try:
            async for msg in claude_agent_sdk.query(prompt=prompt, options=options):
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
        finally:
            if raw is not None:
                raw.close()

        exit_code = 1 if result_evt is None or result_evt.get("is_error") else 0
        text_result = result_evt.get("result") if result_evt else None
        raw_cost = result_evt.get("total_cost_usd") if result_evt else None
        return CompletedRun(
            exit_code=exit_code,
            text_result=text_result,
            events=captured,
            cost_usd=float(raw_cost) if isinstance(raw_cost, (int, float)) else None,
        )

    def reap_all(self) -> None:
        pass

    @property
    def total_cost_usd(self) -> float | None:
        return None


def make_anthropic_client(model: str | None) -> AnthropicSdkClient:
    return AnthropicSdkClient(model)
