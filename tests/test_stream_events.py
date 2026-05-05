from __future__ import annotations

import io
import json

from gremlins.clients.stream import (
    _HANDLERS,
    _emit_event,
    _trunc,
    stream_events,
)


def _bio(*evts):
    return io.BytesIO(b"".join(json.dumps(e).encode() + b"\n" for e in evts))


def test_trunc_truncates():
    assert _trunc("a" * 201) == "a" * 200 + "..."


def test_trunc_newlines():
    assert _trunc("a\nb") == "a b"


def test_trunc_non_string():
    assert _trunc(42) == "42"


def test_trunc_none():
    assert _trunc(None) == ""


def test_init_event_renders(capsys):
    evt = {
        "type": "system",
        "subtype": "init",
        "session_id": "s1",
        "model": "m",
        "cwd": "/x",
    }
    _emit_event(">>", evt)
    assert capsys.readouterr().err == ">>init session=s1 model=m cwd=/x\n"


def test_init_populates_session_id():
    bio = _bio(
        {
            "type": "system",
            "subtype": "init",
            "session_id": "abc",
            "model": "m",
            "cwd": "/",
        }
    )
    sid, _, _, _, _ = stream_events(bio)
    assert sid == "abc"


def test_assistant_text(capsys):
    evt = {
        "type": "assistant",
        "message": {"content": [{"type": "text", "text": "hello"}]},
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "text: hello\n"


def test_assistant_thinking(capsys):
    evt = {
        "type": "assistant",
        "message": {"content": [{"type": "thinking", "thinking": "hm"}]},
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "think: hm\n"


def test_assistant_tool_use_preferred_keys(capsys):
    for key in ("file_path", "command", "pattern", "url", "output_file"):
        evt = {
            "type": "assistant",
            "message": {
                "content": [{"type": "tool_use", "name": "T", "input": {key: "val"}}]
            },
        }
        _emit_event("", evt)
        assert capsys.readouterr().err == "tool: T val\n"


def test_assistant_tool_use_no_arg(capsys):
    evt = {
        "type": "assistant",
        "message": {"content": [{"type": "tool_use", "name": "T", "input": {}}]},
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "tool: T \n"


def test_user_tool_result_str(capsys):
    evt = {
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": "ok"}]},
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "result: ok\n"


def test_user_tool_result_list(capsys):
    body = [{"type": "text", "text": "part1"}, {"type": "text", "text": "part2"}]
    evt = {
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": body}]},
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "result: part1 part2\n"


def test_user_tool_result_none(capsys):
    evt = {
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": None}]},
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "result: \n"


def test_user_tool_result_error(capsys):
    evt = {
        "type": "user",
        "message": {
            "content": [{"type": "tool_result", "is_error": True, "content": "boom"}]
        },
    }
    _emit_event("", evt)
    assert capsys.readouterr().err == "result ERROR: boom\n"


def test_result_event(capsys):
    evt = {
        "type": "result",
        "subtype": "success",
        "num_turns": 3,
        "total_cost_usd": 0.05,
        "result": "done",
    }
    _, cost, result_text, _, _ = stream_events(_bio(evt))
    assert cost == 0.05
    assert result_text == "done"
    assert "final: subtype=success turns=3 cost=0.05" in capsys.readouterr().err


def test_result_cost_fallback():
    evt = {"type": "result", "subtype": "success", "num_turns": 1, "cost_usd": 0.01}
    _, cost, _, _, _ = stream_events(_bio(evt))
    assert cost == 0.01


def test_raw_path_tees_all_lines(tmp_path):
    raw = tmp_path / "raw.jsonl"
    good = json.dumps({"type": "result"}).encode() + b"\n"
    bad = b"not-json\n"
    stream_events(io.BytesIO(good + bad), raw_path=raw)
    assert raw.read_bytes() == good + bad


def test_malformed_json_skipped():
    good = json.dumps({"type": "result", "total_cost_usd": 0.02}).encode() + b"\n"
    _, cost, _, _, _ = stream_events(io.BytesIO(b"bad\n" + good))
    assert cost == 0.02


def test_capture_true_returns_events():
    evt = {"type": "result", "subtype": "success", "num_turns": 1}
    _, _, _, events, _ = stream_events(_bio(evt), capture=True)
    assert events == [evt]


def test_capture_false_returns_none():
    _, _, _, events, _ = stream_events(_bio({"type": "result"}), capture=False)
    assert events is None


def test_handler_exception_swallowed(monkeypatch):
    def boom(prefix, evt):
        raise RuntimeError("boom")

    monkeypatch.setitem(_HANDLERS, "system", boom)
    evt1 = {"type": "system", "subtype": "init", "session_id": "s"}
    evt2 = {"type": "result", "subtype": "ok", "num_turns": 1}
    sid, _, _, events, _ = stream_events(_bio(evt1, evt2), capture=True)
    assert len(events) == 2
    assert sid == "s"


def test_stream_events_timed_out_false_on_normal_output():
    evt = {"type": "result", "subtype": "success", "num_turns": 1}
    _, _, _, _, timed_out = stream_events(_bio(evt))
    assert timed_out is False


def test_stream_events_timed_out_true_on_timeout_line():
    good = json.dumps({"type": "result"}).encode() + b"\n"
    timeout_line = b"API Error: Stream idle timeout\n"
    _, _, _, _, timed_out = stream_events(io.BytesIO(timeout_line + good))
    assert timed_out is True


def test_stream_events_timed_out_true_in_json_line():
    # timeout message embedded in a JSON line (unlikely but covers the check)
    line = (
        json.dumps({"type": "result", "result": "Stream idle timeout"}).encode() + b"\n"
    )
    _, _, _, _, timed_out = stream_events(io.BytesIO(line))
    assert timed_out is True
