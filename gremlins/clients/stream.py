from __future__ import annotations

import json
import pathlib
import sys
from typing import IO, Any, cast


def _trunc(s: object, n: int = 200) -> str:
    if s is None:
        return ""
    if not isinstance(s, str):
        s = str(s)
    s = s.replace("\n", " ")
    return s[:n] + "..." if len(s) > n else s


def _emit_event(prefix: str, evt: dict[str, Any]) -> None:
    t = evt.get("type")
    out = sys.stderr
    if t == "system":
        if evt.get("subtype") != "init":
            return
        out.write(
            f"{prefix}init session={evt.get('session_id', '?')} "
            f"model={evt.get('model', '?')} cwd={evt.get('cwd', '?')}\n"
        )
    elif t == "assistant":
        msg = cast(dict[str, Any], evt.get("message") or {})
        content = cast(list[dict[str, Any]], msg.get("content") or [])
        for c in content:
            ct = c.get("type")
            if ct == "text":
                out.write(f"{prefix}text: {_trunc(c.get('text', ''))}\n")
            elif ct == "thinking":
                thought = str(c.get("thinking") or "")
                out.write(f"{prefix}think: {_trunc(thought)}\n")
            elif ct == "tool_use":
                inp = cast(dict[str, Any], c.get("input") or {})
                arg = ""
                for k in ("file_path", "command", "pattern", "url", "output_file"):
                    v = inp.get(k)
                    if v:
                        arg = str(v)
                        break
                out.write(f"{prefix}tool: {c.get('name', '?')} {_trunc(str(arg))}\n")
    elif t == "user":
        msg = cast(dict[str, Any], evt.get("message") or {})
        content = cast(list[dict[str, Any]], msg.get("content") or [])
        for c in content:
            if c.get("type") != "tool_result":
                continue
            err = " ERROR" if c.get("is_error") is True else ""
            body = c.get("content")
            if isinstance(body, list):
                body_s = " ".join(
                    str(cast(dict[str, Any], p).get("text") or "")
                    for p in cast(list[Any], body)
                    if isinstance(p, dict)
                )
            elif isinstance(body, str):
                body_s = body
            elif body is None:
                body_s = ""
            else:
                body_s = str(body)
            out.write(f"{prefix}result{err}: {_trunc(body_s)}\n")
    elif t == "result":
        cost = evt.get("total_cost_usd", evt.get("cost_usd", "?"))
        out.write(
            f"{prefix}final: subtype={evt.get('subtype', '?')} "
            f"turns={evt.get('num_turns', '?')} cost={cost}\n"
        )
    out.flush()


def stream_events(
    stdout: IO[bytes],
    *,
    prefix: str = "",
    raw_path: pathlib.Path | None = None,
    capture: bool = False,
) -> tuple[str | None, float | None, str | None, list[dict[str, Any]] | None]:
    """Read stream-json lines from stdout, render via _emit_event.

    Returns (session_id, cost_usd, result_text, events).
    """
    session_id: str | None = None
    cost_usd: float | None = None
    result_text: str | None = None
    events: list[dict[str, Any]] | None = [] if capture else None

    raw = None
    if raw_path is not None:
        raw = open(raw_path, "ab")
    try:
        for line in stdout:
            if raw is not None:
                raw.write(line)
                raw.flush()
            try:
                evt = json.loads(line.decode("utf-8", errors="replace"))
            except Exception:
                continue
            if not isinstance(evt, dict):
                continue
            evt = cast(dict[str, Any], evt)
            if (
                session_id is None
                and evt.get("type") == "system"
                and evt.get("subtype") == "init"
            ):
                sid = evt.get("session_id")
                if isinstance(sid, str):
                    session_id = sid
            if evt.get("type") == "result":
                raw_cost = evt.get("total_cost_usd", evt.get("cost_usd"))
                if isinstance(raw_cost, (int, float)):
                    cost_usd = float(raw_cost)
                raw_result = evt.get("result")
                if isinstance(raw_result, str):
                    result_text = raw_result
            if events is not None:
                events.append(evt)
            try:
                _emit_event(prefix, evt)
            except Exception:
                pass
    finally:
        if raw is not None:
            raw.close()
    return session_id, cost_usd, result_text, events
