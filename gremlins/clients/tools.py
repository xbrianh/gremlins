"""File-system and shell tools for OpenAI agents: Read, Edit, Bash, Write, Grep, Glob."""

from __future__ import annotations

import asyncio
import fnmatch
import json
import os
import pathlib
import re
from typing import Any, cast

from agents import FunctionTool, Tool
from agents.tool_context import ToolContext


def _cwd(ctx: ToolContext[Any]) -> str | None:
    c = cast("dict[str, object]", ctx.context)
    return cast("str | None", c.get("cwd"))


def _extra_env(ctx: ToolContext[Any]) -> dict[str, str] | None:
    c = cast("dict[str, object]", ctx.context)
    return cast("dict[str, str] | None", c.get("extra_env"))


def _resolve(file_path: str, cwd: str | None) -> pathlib.Path:
    p = pathlib.Path(file_path)
    if not p.is_absolute() and cwd is not None:
        return pathlib.Path(cwd) / p
    return p


async def _read_invoke(ctx: ToolContext[Any], args_json: str) -> str:
    args: dict[str, Any] = json.loads(args_json)
    path = _resolve(args["file_path"], _cwd(ctx))
    try:
        lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    except OSError as e:
        return f"Error: {e}"
    offset: int = args.get("offset", 0) or 0
    limit: int | None = args.get("limit")
    lines = lines[offset:]
    if limit is not None:
        lines = lines[:limit]
    return "".join(lines)


async def _edit_invoke(ctx: ToolContext[Any], args_json: str) -> str:
    args: dict[str, Any] = json.loads(args_json)
    path = _resolve(args["file_path"], _cwd(ctx))
    old, new = args["old_string"], args["new_string"]
    try:
        content = path.read_text(encoding="utf-8")
    except OSError as e:
        return f"Error: {e}"
    if old not in content:
        return f"Error: old_string not found in {args['file_path']}"
    if content.count(old) > 1:
        return f"Error: old_string is not unique in {args['file_path']}"
    path.write_text(content.replace(old, new, 1), encoding="utf-8")
    return "OK"


async def _bash_invoke(ctx: ToolContext[Any], args_json: str) -> str:
    args: dict[str, Any] = json.loads(args_json)
    extra = _extra_env(ctx)
    env = {**os.environ, **extra} if extra else None
    proc = await asyncio.create_subprocess_shell(
        args["command"],
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
        cwd=_cwd(ctx),
        env=env,
    )
    try:
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=120)
    except TimeoutError:
        proc.kill()
        return "[timeout]"
    output = stdout.decode(errors="replace")
    rc = proc.returncode
    if rc != 0:
        return f"[exit {rc}]\n{output}"
    return output


async def _write_invoke(ctx: ToolContext[Any], args_json: str) -> str:
    args: dict[str, Any] = json.loads(args_json)
    path = _resolve(args["file_path"], _cwd(ctx))
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(args["content"], encoding="utf-8")
    except OSError as e:
        return f"Error: {e}"
    return "OK"


_GREP_MAX_LINES = 2000
_SKIP_DIRS = {"__pycache__", "node_modules"}


async def _grep_invoke(ctx: ToolContext[Any], args_json: str) -> str:
    args: dict[str, Any] = json.loads(args_json)
    try:
        pattern = re.compile(args["pattern"])
    except re.error as e:
        return f"Error: invalid regex: {e}"

    base = _resolve(args.get("path", "."), _cwd(ctx))
    if not base.exists():
        return f"Error: path does not exist: {base}"

    glob_filter: str | None = args.get("glob") or None
    matches: list[str] = []
    truncated = False

    def _scan_dir_file(file_path: pathlib.Path) -> None:
        nonlocal truncated
        try:
            if b"\x00" in file_path.read_bytes()[:8192]:
                return
            with file_path.open(encoding="utf-8", errors="replace") as f:
                for lineno, line in enumerate(f, 1):
                    if pattern.search(line):
                        matches.append(f"{file_path}:{lineno}:{line.rstrip()}")
                        if len(matches) >= _GREP_MAX_LINES:
                            truncated = True
                            return
        except OSError:
            pass

    if base.is_file():
        if glob_filter is None or fnmatch.fnmatch(base.name, glob_filter):
            try:
                if b"\x00" not in base.read_bytes()[:8192]:
                    with base.open(encoding="utf-8", errors="replace") as f:
                        for lineno, line in enumerate(f, 1):
                            if pattern.search(line):
                                matches.append(f"{base}:{lineno}:{line.rstrip()}")
                                if len(matches) >= _GREP_MAX_LINES:
                                    truncated = True
                                    break
            except OSError as e:
                return f"Error: {e}"
    else:
        for root, dirs, files in os.walk(base):
            dirs[:] = sorted(
                d for d in dirs if not d.startswith(".") and d not in _SKIP_DIRS
            )
            for name in sorted(files):
                if truncated:
                    break
                if glob_filter is None or fnmatch.fnmatch(name, glob_filter):
                    _scan_dir_file(pathlib.Path(root) / name)
            if truncated:
                break

    if not matches:
        return "(no matches)"
    result = "\n".join(matches)
    if truncated:
        result += f"\n[truncated at {_GREP_MAX_LINES} matches]"
    return result


async def _glob_invoke(ctx: ToolContext[Any], args_json: str) -> str:
    args: dict[str, Any] = json.loads(args_json)
    base = pathlib.Path(_cwd(ctx) or ".") / args.get("path", ".")
    matches = sorted(base.glob(args["pattern"]))
    return "\n".join(str(m) for m in matches) or "(no matches)"


GREMLINS_TOOLS: list[Tool] = [
    FunctionTool(
        name="Read",
        description="Read a file from the filesystem.",
        params_json_schema={
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute or relative path",
                },
                "limit": {"type": "integer", "description": "Max lines to read"},
                "offset": {
                    "type": "integer",
                    "description": "Line offset to start from",
                },
            },
            "required": ["file_path"],
            "additionalProperties": False,
        },
        on_invoke_tool=_read_invoke,
        strict_json_schema=False,
    ),
    FunctionTool(
        name="Edit",
        description="Replace old_string with new_string in a file (first occurrence).",
        params_json_schema={
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": False,
        },
        on_invoke_tool=_edit_invoke,
        strict_json_schema=False,
    ),
    FunctionTool(
        name="Bash",
        description="Run a shell command and return combined stdout/stderr.",
        params_json_schema={
            "type": "object",
            "properties": {
                "command": {"type": "string"},
            },
            "required": ["command"],
            "additionalProperties": False,
        },
        on_invoke_tool=_bash_invoke,
        strict_json_schema=False,
    ),
    FunctionTool(
        name="Write",
        description="Write content to a file, creating it if necessary.",
        params_json_schema={
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["file_path", "content"],
            "additionalProperties": False,
        },
        on_invoke_tool=_write_invoke,
        strict_json_schema=False,
    ),
    FunctionTool(
        name="Grep",
        description="Search file contents using a regex pattern.",
        params_json_schema={
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {
                    "type": "string",
                    "description": "Directory or file to search",
                },
                "glob": {"type": "string", "description": "Glob filter for file names"},
            },
            "required": ["pattern"],
            "additionalProperties": False,
        },
        on_invoke_tool=_grep_invoke,
        strict_json_schema=False,
    ),
    FunctionTool(
        name="Glob",
        description="Find files matching a glob pattern.",
        params_json_schema={
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {
                    "type": "string",
                    "description": "Base directory to search in",
                },
            },
            "required": ["pattern"],
            "additionalProperties": False,
        },
        on_invoke_tool=_glob_invoke,
        strict_json_schema=False,
    ),
]
