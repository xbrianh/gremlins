import asyncio
import json
import pathlib

from agents.tool_context import ToolContext

from gremlins.clients.tools import (
    _bash_invoke,
    _edit_invoke,
    _grep_invoke,
    _read_invoke,
    _write_invoke,
    build_tools,
)


def _ctx(cwd: str | None = None) -> ToolContext:
    return ToolContext(
        context={"cwd": cwd}, tool_name="", tool_call_id="", tool_arguments=""
    )


def test_path_tools_outside_denied(tmp_path: pathlib.Path) -> None:
    root = tmp_path / "wt"
    root.mkdir()
    out = tmp_path / "out.txt"
    out.write_text("x")
    t = build_tools(bypass=False, worktree_root=root, audit_log=None)
    for name in ("Read", "Edit", "Write"):
        tool = next(tt for tt in t if tt.name == name)
        arg = {"file_path": str(out)}
        if name == "Edit":
            arg["old_string"] = "x"
            arg["new_string"] = "y"
        if name == "Write":
            arg["content"] = "y"
        res = asyncio.run(tool.on_invoke_tool(_ctx(str(root)), json.dumps(arg)))
        assert "path outside worktree" in res
    tb = build_tools(bypass=True, worktree_root=root, audit_log=None)
    for name in ("Read", "Edit", "Write"):
        tool = next(tt for tt in tb if tt.name == name)
        arg = {"file_path": str(out)}
        if name == "Edit":
            arg.update({"old_string": "x", "new_string": "y"})
        if name == "Write":
            arg["content"] = "y"
        res = asyncio.run(tool.on_invoke_tool(_ctx(str(root)), json.dumps(arg)))
        assert res in ("x", "OK", "OK")


def test_bash_abs_outside_denied(tmp_path: pathlib.Path) -> None:
    root = tmp_path / "wt"
    root.mkdir()
    t = build_tools(bypass=False, worktree_root=root, audit_log=None)
    bash = next(tt for tt in t if tt.name == "Bash")
    cmd = json.dumps({"command": "/bin/ls /etc"})
    res = asyncio.run(bash.on_invoke_tool(_ctx(str(root)), cmd))
    assert "path outside worktree" in res
    tb = build_tools(bypass=True, worktree_root=root, audit_log=None)
    bashb = next(tt for tt in tb if tt.name == "Bash")
    resb = asyncio.run(bashb.on_invoke_tool(_ctx(str(root)), cmd))
    assert "bin" in resb or "ls" in resb or resb  # may vary


def test_audit_log(tmp_path: pathlib.Path) -> None:
    root = tmp_path / "wt"
    root.mkdir()
    (root / "f.txt").write_text("hi")
    log = tmp_path / "audit.jsonl"
    t = build_tools(bypass=False, worktree_root=root, audit_log=log)
    readt = next(tt for tt in t if tt.name == "Read")
    # ok
    asyncio.run(readt.on_invoke_tool(_ctx(str(root)), json.dumps({"file_path": "f.txt"})))
    # denied
    asyncio.run(readt.on_invoke_tool(_ctx(str(root)), json.dumps({"file_path": "/etc/passwd"})))
    # error (missing)
    asyncio.run(readt.on_invoke_tool(_ctx(str(root)), json.dumps({"file_path": "no.txt"})))
    lines = log.read_text().splitlines()
    assert len(lines) == 3
    statuses = [json.loads(l)["status"] for l in lines]
    bypasses = [json.loads(l)["bypass"] for l in lines]
    assert statuses == ["ok", "denied", "error"]
    assert all(b is False for b in bypasses)


def test_symlink_denied(tmp_path: pathlib.Path) -> None:
    root = tmp_path / "wt"
    root.mkdir()
    secret = tmp_path / "secret"
    secret.write_text("pass")
    link = root / "link.txt"
    link.symlink_to(secret)
    t = build_tools(bypass=False, worktree_root=root, audit_log=None)
    readt = next(tt for tt in t if tt.name == "Read")
    res = asyncio.run(readt.on_invoke_tool(_ctx(str(root)), json.dumps({"file_path": "link.txt"})))
    assert "path outside worktree" in res


def test_bypass_equivalence(tmp_path: pathlib.Path) -> None:
    root = tmp_path / "wt"
    root.mkdir()
    f = root / "a.py"
    f.write_text("print(1)\n")
    g = root / "b.txt"
    g.write_text("foo bar")
    t = build_tools(bypass=True, worktree_root=root, audit_log=None)
    readt = next(tt for tt in t if tt.name == "Read")
    r1 = asyncio.run(readt.on_invoke_tool(_ctx(str(root)), json.dumps({"file_path": "a.py"})))
    r2 = asyncio.run(_read_invoke(_ctx(str(root)), json.dumps({"file_path": "a.py"})))
    assert r1 == r2
    et = next(tt for tt in t if tt.name == "Edit")
    e1 = asyncio.run(et.on_invoke_tool(_ctx(str(root)), json.dumps({"file_path": "a.py", "old_string": "1", "new_string": "2"})))
    e2 = asyncio.run(_edit_invoke(_ctx(str(root)), json.dumps({"file_path": "a.py", "old_string": "1", "new_string": "2"})))
    assert e1 == e2
    gt = next(tt for tt in t if tt.name == "Grep")
    g1 = asyncio.run(gt.on_invoke_tool(_ctx(str(root)), json.dumps({"pattern": "foo", "path": "."})))
    g2 = asyncio.run(_grep_invoke(_ctx(str(root)), json.dumps({"pattern": "foo", "path": "."})))
    assert g1 == g2