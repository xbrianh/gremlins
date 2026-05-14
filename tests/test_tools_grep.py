"""Tests for the pure-Python _grep_invoke."""

from __future__ import annotations

import asyncio
import json
from unittest.mock import MagicMock

from gremlins.clients.tools import _grep_invoke


def _ctx(cwd: str | None = None) -> MagicMock:
    ctx = MagicMock()
    ctx.context = {"cwd": cwd} if cwd else {}
    return ctx


def _run(coro):
    return asyncio.run(coro)


def test_grep_match(tmp_path):
    (tmp_path / "a.py").write_text("hello world\nno match here\nhello again\n")
    result = _run(_grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "hello"})))
    assert "a.py:1:hello world" in result
    assert "a.py:3:hello again" in result
    assert "no match" not in result


def test_grep_glob_filter(tmp_path):
    (tmp_path / "keep.py").write_text("target line\n")
    (tmp_path / "skip.txt").write_text("target line\n")
    result = _run(
        _grep_invoke(
            _ctx(str(tmp_path)),
            json.dumps({"pattern": "target", "glob": "*.py"}),
        )
    )
    assert "keep.py" in result
    assert "skip.txt" not in result


def test_grep_bad_regex(tmp_path):
    result = _run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "[invalid"}))
    )
    assert result.startswith("Error: invalid regex")


def test_grep_no_matches(tmp_path):
    (tmp_path / "f.txt").write_text("nothing interesting\n")
    result = _run(_grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "xyz123"})))
    assert result == "(no matches)"
