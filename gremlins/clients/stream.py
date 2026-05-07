from __future__ import annotations

import json
import pathlib
import queue
import sys
import threading
from collections.abc import Callable
from typing import IO, Any, cast

STREAM_IDLE_TIMEOUT = 120


def _trunc(s: object, n: int = 200) -> str:
    if s is None:
        return ""
    if not isinstance(s, str):
        s = str(s)
    s = s.replace("\n", " ")
    return s[:n] + "..." if len(s) > n else s


def _fmt_text(c: dict[str, Any]) -> str:
    return f"text: {_trunc(c.get('text', ''))}"


def _fmt_thinking(c: dict[str, Any]) -> str:
    return f"think: {_trunc(c.get('thinking') or '')}"


def _fmt_tool_use(c: dict[str, Any]) -> str:
    inp = cast(dict[str, Any], c.get("input") or {})
    arg = ""
    for k in ("file_path", "command", "pattern", "url", "output_file"):
        if inp.get(k):
            arg = str(inp[k])
            break
    return f"tool: {c.get('name', '?')} {_trunc(arg)}"


_CONTENT_FMT = {"text": _fmt_text, "thinking": _fmt_thinking, "tool_use": _fmt_tool_use}


def _tool_result_body(body: Any) -> str:
    if isinstance(body, list):
        return " ".join(
            str(cast(dict[str, Any], p).get("text") or "")
            for p in cast(list[Any], body)
            if isinstance(p, dict)
        )
    if isinstance(body, str):
        return body
    if body is None:
        return ""
    return str(body)


def _emit_init(prefix: str, evt: dict[str, Any]) -> None:
    if evt.get("subtype") != "init":
        return
    sys.stderr.write(
        f"{prefix}init model={evt.get('model', '?')} cwd={evt.get('cwd', '?')}\n"
    )


def _emit_assistant(prefix: str, evt: dict[str, Any]) -> None:
    msg = cast(dict[str, Any], evt.get("message") or {})
    content = cast(list[dict[str, Any]], msg.get("content") or [])
    for c in content:
        fmt = _CONTENT_FMT.get(str(c.get("type") or ""))
        if fmt:
            sys.stderr.write(f"{prefix}{fmt(c)}\n")


def _emit_user(prefix: str, evt: dict[str, Any]) -> None:
    msg = cast(dict[str, Any], evt.get("message") or {})
    content = cast(list[dict[str, Any]], msg.get("content") or [])
    for c in content:
        if c.get("type") != "tool_result":
            continue
        err = " ERROR" if c.get("is_error") is True else ""
        sys.stderr.write(
            f"{prefix}result{err}: {_trunc(_tool_result_body(c.get('content')))}\n"
        )


def _emit_result(prefix: str, evt: dict[str, Any]) -> None:
    cost = evt.get("total_cost_usd", evt.get("cost_usd", "?"))
    sys.stderr.write(
        f"{prefix}final: subtype={evt.get('subtype', '?')} "
        f"turns={evt.get('num_turns', '?')} cost={cost}\n"
    )


_HANDLERS: dict[str, Callable[[str, dict[str, Any]], None]] = {
    "system": _emit_init,
    "assistant": _emit_assistant,
    "user": _emit_user,
    "result": _emit_result,
}


def _emit_event(prefix: str, evt: dict[str, Any]) -> None:
    handler = _HANDLERS.get(str(evt.get("type") or ""))
    if handler:
        handler(prefix, evt)
    sys.stderr.flush()


def _decode_line(line: bytes) -> dict[str, Any] | None:
    try:
        evt = json.loads(line.decode("utf-8", errors="replace"))
    except Exception:
        return None
    return cast(dict[str, Any], evt) if isinstance(evt, dict) else None


def _extract_state(evt: dict[str, Any], state: dict[str, Any]) -> None:
    if evt.get("type") == "result":
        raw_cost = evt.get("total_cost_usd", evt.get("cost_usd"))
        if isinstance(raw_cost, (int, float)):
            state["cost_usd"] = float(raw_cost)
        raw_result = evt.get("result")
        if isinstance(raw_result, str):
            state["result_text"] = raw_result


def stream_events(
    stdout: IO[bytes],
    *,
    prefix: str = "",
    raw_path: pathlib.Path | None = None,
    capture: bool = False,
    idle_timeout: float = STREAM_IDLE_TIMEOUT,
) -> tuple[float | None, str | None, list[dict[str, Any]] | None, bool]:
    state: dict[str, Any] = {"cost_usd": None, "result_text": None}
    events: list[dict[str, Any]] | None = [] if capture else None
    timed_out = False
    raw = open(raw_path, "ab") if raw_path is not None else None

    q: queue.Queue[bytes | None] = queue.Queue()

    def _reader() -> None:
        try:
            for line in stdout:
                q.put(line)
        finally:
            q.put(None)

    t = threading.Thread(target=_reader, daemon=True)
    t.start()

    try:
        while True:
            try:
                line = q.get(timeout=idle_timeout)
            except queue.Empty:
                timed_out = True
                try:
                    stdout.close()
                except OSError:
                    pass
                break
            if line is None:
                break
            if raw is not None:
                raw.write(line)
                raw.flush()
            if b"Stream idle timeout" in line and _decode_line(line) is None:
                timed_out = True
            evt = _decode_line(line)
            if evt is None:
                continue
            _extract_state(evt, state)
            if events is not None:
                events.append(evt)
            try:
                _emit_event(prefix, evt)
            except Exception:
                pass
    finally:
        t.join(timeout=5.0)
        if raw is not None:
            raw.close()
    return (
        state["cost_usd"],
        state["result_text"],
        events,
        timed_out,
    )
