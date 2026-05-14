from __future__ import annotations

import asyncio
import json
from unittest.mock import MagicMock

from gremlins.clients.tools import _grep_invoke


def _ctx(cwd: str | None = None) -> MagicMock:
    ctx = MagicMock()
    ctx.context = {"cwd": cwd} if cwd else {}
    return ctx


def test_grep_match(tmp_path):
    (tmp_path / "a.py").write_text("hello world\nno match here\nhello again\n")
    result = asyncio.run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "hello"}))
    )
    assert "a.py:1:hello world" in result
    assert "a.py:3:hello again" in result
    assert "no match" not in result


def test_grep_glob_filter(tmp_path):
    (tmp_path / "keep.py").write_text("target line\n")
    (tmp_path / "skip.txt").write_text("target line\n")
    result = asyncio.run(
        _grep_invoke(
            _ctx(str(tmp_path)),
            json.dumps({"pattern": "target", "glob": "*.py"}),
        )
    )
    assert "keep.py" in result
    assert "skip.txt" not in result


def test_grep_bad_regex(tmp_path):
    result = asyncio.run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "[invalid"}))
    )
    assert result.startswith("Error: invalid regex")


def test_grep_no_matches(tmp_path):
    (tmp_path / "f.txt").write_text("nothing interesting\n")
    result = asyncio.run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "xyz123"}))
    )
    assert result == "(no matches)"


def test_grep_truncation(tmp_path, monkeypatch):
    import gremlins.clients.tools as tools_mod

    monkeypatch.setattr(tools_mod, "_GREP_MAX_LINES", 5)
    content = "\n".join(f"match {i}" for i in range(10)) + "\n"
    (tmp_path / "big.txt").write_text(content)
    result = asyncio.run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "match"}))
    )
    lines = result.split("\n")
    assert lines[-1] == "[truncated at 5 matches]"
    assert len(lines) == 6  # 5 match lines + truncation notice


def test_grep_skips_hidden_dirs(tmp_path):
    hidden = tmp_path / ".git"
    hidden.mkdir()
    (hidden / "config").write_text("target\n")
    (tmp_path / "visible.py").write_text("target\n")
    result = asyncio.run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "target"}))
    )
    assert ".git" not in result
    assert "visible.py" in result


def test_grep_skips_binary_files(tmp_path):
    (tmp_path / "binary.bin").write_bytes(b"match\x00binary")
    (tmp_path / "text.py").write_text("match text\n")
    result = asyncio.run(
        _grep_invoke(_ctx(str(tmp_path)), json.dumps({"pattern": "match"}))
    )
    assert "binary.bin" not in result
    assert "text.py" in result
