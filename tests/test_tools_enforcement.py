"""Tests for build_tools enforcement: path scoping, bash checks, audit JSONL, bypass."""

from __future__ import annotations

import asyncio
import json
import pathlib
from typing import Any
from unittest.mock import MagicMock

import pytest
from agents import FunctionTool

from gremlins.clients.tools import (  # pyright: ignore[reportPrivateUsage]
    _audit,
    _bash_check,
    _enforce,
    _within_worktree,
    build_tools,
)


def _ctx(cwd: str | None = None) -> Any:
    mock = MagicMock()
    mock.context = {} if cwd is None else {"cwd": cwd}
    return mock


# ---------------------------------------------------------------------------
# _within_worktree
# ---------------------------------------------------------------------------


def test_within_worktree_same_dir(tmp_path: pathlib.Path) -> None:
    assert _within_worktree(tmp_path, tmp_path)


def test_within_worktree_child(tmp_path: pathlib.Path) -> None:
    child = tmp_path / "sub" / "file.txt"
    assert _within_worktree(child, tmp_path)


def test_within_worktree_outside(tmp_path: pathlib.Path) -> None:
    assert not _within_worktree(tmp_path, tmp_path / "sub")


def test_within_worktree_sibling(tmp_path: pathlib.Path) -> None:
    sibling = tmp_path.parent / "other"
    assert not _within_worktree(sibling, tmp_path)


def test_within_worktree_symlink_traversal(tmp_path: pathlib.Path) -> None:
    worktree = tmp_path / "worktree"
    worktree.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    link = worktree / "link"
    link.symlink_to(outside)
    assert not _within_worktree(link, worktree)


# ---------------------------------------------------------------------------
# _enforce
# ---------------------------------------------------------------------------


def test_enforce_bypass_allows_outside(tmp_path: pathlib.Path) -> None:
    outside = str(tmp_path.parent / "other" / "file.txt")
    assert _enforce(True, tmp_path, outside, None) is None


def test_enforce_inside_worktree(tmp_path: pathlib.Path) -> None:
    inside = str(tmp_path / "file.txt")
    assert _enforce(False, tmp_path, inside, None) is None


def test_enforce_outside_worktree(tmp_path: pathlib.Path) -> None:
    outside = str(tmp_path.parent / "secret.txt")
    err = _enforce(False, tmp_path, outside, None)
    assert err is not None
    assert "outside worktree" in err


def test_enforce_relative_path_with_cwd(tmp_path: pathlib.Path) -> None:
    err = _enforce(False, tmp_path, "file.txt", str(tmp_path))
    assert err is None


def test_enforce_relative_path_escapes_via_cwd(tmp_path: pathlib.Path) -> None:
    # cwd is inside worktree but ../../.. escapes it
    err = _enforce(False, tmp_path, "../../../etc/passwd", str(tmp_path))
    assert err is not None
    assert "outside worktree" in err


# ---------------------------------------------------------------------------
# _bash_check
# ---------------------------------------------------------------------------


def test_bash_check_bypass_allows_absolute(tmp_path: pathlib.Path) -> None:
    cmd = "cat /etc/passwd"
    assert _bash_check(True, tmp_path, cmd, None) is None


def test_bash_check_safe_command(tmp_path: pathlib.Path) -> None:
    assert _bash_check(False, tmp_path, "ls -la", str(tmp_path)) is None


def test_bash_check_absolute_outside(tmp_path: pathlib.Path) -> None:
    cmd = "cat /etc/passwd"
    err = _bash_check(False, tmp_path, cmd, str(tmp_path))
    assert err is not None
    assert "outside worktree" in err


def test_bash_check_absolute_inside(tmp_path: pathlib.Path) -> None:
    inside = str(tmp_path / "file.txt")
    cmd = f"cat {inside}"
    assert _bash_check(False, tmp_path, cmd, str(tmp_path)) is None


def test_bash_check_tilde_expansion_outside(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("HOME", "/nonexistent-home")
    err = _bash_check(False, tmp_path, "cat ~/.ssh/id_rsa", str(tmp_path))
    assert err is not None
    assert "outside worktree" in err


def test_bash_check_dotdot_escapes(tmp_path: pathlib.Path) -> None:
    err = _bash_check(False, tmp_path, "cat ../secret", str(tmp_path))
    assert err is not None
    assert "outside worktree" in err


def test_bash_check_intermediate_traversal(tmp_path: pathlib.Path) -> None:
    # subdir/../../etc/passwd doesn't start with /, ~, or .., but still escapes
    err = _bash_check(False, tmp_path, "cat subdir/../../etc/passwd", str(tmp_path))
    assert err is not None
    assert "outside worktree" in err


def test_bash_check_empty_command(tmp_path: pathlib.Path) -> None:
    assert _bash_check(False, tmp_path, "", str(tmp_path)) is None
    assert _bash_check(False, tmp_path, "   ", str(tmp_path)) is None


# ---------------------------------------------------------------------------
# _audit
# ---------------------------------------------------------------------------


def test_audit_writes_jsonl(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    _audit(log, "Read", "/some/file", "ok", False)
    lines = log.read_text().splitlines()
    assert len(lines) == 1
    entry = json.loads(lines[0])
    assert entry["tool"] == "Read"
    assert entry["key_arg"] == "/some/file"
    assert entry["status"] == "ok"
    assert entry["bypass"] is False
    assert "ts" in entry


def test_audit_appends_multiple(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    _audit(log, "Read", "a", "ok", False)
    _audit(log, "Bash", "b", "denied", False)
    lines = log.read_text().splitlines()
    assert len(lines) == 2
    assert json.loads(lines[1])["status"] == "denied"


def test_audit_none_log_is_noop(tmp_path: pathlib.Path) -> None:
    _audit(None, "Read", "/file", "ok", False)  # must not raise


def test_audit_truncates_key_arg(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    long_arg = "x" * 300
    _audit(log, "Read", long_arg, "ok", False)
    entry = json.loads(log.read_text())
    assert len(entry["key_arg"]) == 200


def test_audit_bypass_flag(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    _audit(log, "Bash", "cmd", "ok", True)
    entry = json.loads(log.read_text())
    assert entry["bypass"] is True


# ---------------------------------------------------------------------------
# _wrap — integration via build_tools
# ---------------------------------------------------------------------------


def _ft(tools: list[Any], name: str) -> FunctionTool:
    return next(t for t in tools if t.name == name)  # type: ignore[return-value]


def test_wrap_denied_writes_audit(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    tools = build_tools(bypass=False, worktree_root=tmp_path, audit_log=log)
    read_tool = _ft(tools, "Read")
    outside = str(tmp_path.parent / "secret.txt")
    ctx = _ctx(cwd=str(tmp_path))
    result = asyncio.run(
        read_tool.on_invoke_tool(ctx, json.dumps({"file_path": outside}))
    )
    assert "outside worktree" in result
    entry = json.loads(log.read_text())
    assert entry["status"] == "denied"
    assert entry["tool"] == "Read"


def test_wrap_ok_writes_audit(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    target = tmp_path / "hello.txt"
    target.write_text("hi")
    tools = build_tools(bypass=False, worktree_root=tmp_path, audit_log=log)
    read_tool = _ft(tools, "Read")
    ctx = _ctx(cwd=str(tmp_path))
    result = asyncio.run(
        read_tool.on_invoke_tool(ctx, json.dumps({"file_path": str(target)}))
    )
    assert result == "hi"
    entry = json.loads(log.read_text())
    assert entry["status"] == "ok"


def test_wrap_invalid_json_writes_error(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    tools = build_tools(bypass=False, worktree_root=tmp_path, audit_log=log)
    read_tool = _ft(tools, "Read")
    ctx = _ctx()
    result = asyncio.run(read_tool.on_invoke_tool(ctx, "not-json{{{{"))
    assert "Error" in result
    entry = json.loads(log.read_text())
    assert entry["status"] == "error"


def test_wrap_bypass_skips_enforcement(tmp_path: pathlib.Path) -> None:
    worktree = tmp_path / "worktree"
    worktree.mkdir()
    log = worktree / "audit.jsonl"
    outside = tmp_path / "outside.txt"
    outside.write_text("sensitive")
    tools = build_tools(bypass=True, worktree_root=worktree, audit_log=log)
    read_tool = _ft(tools, "Read")
    ctx = _ctx()
    result = asyncio.run(
        read_tool.on_invoke_tool(ctx, json.dumps({"file_path": str(outside)}))
    )
    assert result == "sensitive"
    entry = json.loads(log.read_text())
    assert entry["status"] == "ok"
    assert entry["bypass"] is True


def test_wrap_bash_denied_writes_audit(tmp_path: pathlib.Path) -> None:
    log = tmp_path / "audit.jsonl"
    tools = build_tools(bypass=False, worktree_root=tmp_path, audit_log=log)
    bash_tool = _ft(tools, "Bash")
    ctx = _ctx(cwd=str(tmp_path))
    result = asyncio.run(
        bash_tool.on_invoke_tool(ctx, json.dumps({"command": "cat /etc/passwd"}))
    )
    assert "outside worktree" in result
    entry = json.loads(log.read_text())
    assert entry["status"] == "denied"
    assert entry["tool"] == "Bash"
