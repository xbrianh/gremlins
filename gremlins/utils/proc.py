from __future__ import annotations

import asyncio
import logging
import os
import pathlib
import subprocess
import sys
from collections.abc import AsyncIterator
from typing import Any

from _gremlins_core.utils.proc import (
    run as _run,
)
from _gremlins_core.utils.proc import (
    run_async as _run_async,
)
from _gremlins_core.utils.proc import (
    run_ok as _run_ok,
)
from _gremlins_core.utils.proc import (
    run_ok_async as _run_ok_async,
)
from _gremlins_core.utils.proc import (
    run_or_raise as _run_or_raise,
)
from _gremlins_core.utils.proc import (
    run_quiet as _run_quiet,
)
from _gremlins_core.utils.proc import (
    run_shell_async as _run_shell_async,
)
from _gremlins_core.utils.proc import (
    terminate_with_grace as _terminate_with_grace,
)

logger = logging.getLogger(__name__)


def run(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        r = _run(cmd, cwd=_to_str(cwd), check=check, timeout=timeout)
    except subprocess.CalledProcessError as e:
        if text:
            raise subprocess.CalledProcessError(
                e.returncode,
                e.cmd,
                e.stdout.decode() if isinstance(e.stdout, bytes) else e.stdout,
                e.stderr.decode() if isinstance(e.stderr, bytes) else e.stderr,
            ) from None
        raise
    except subprocess.TimeoutExpired as e:
        if text:
            raise subprocess.TimeoutExpired(
                e.cmd,
                e.timeout,
                e.stdout.decode() if isinstance(e.stdout, bytes) else e.stdout,
                e.stderr.decode() if isinstance(e.stderr, bytes) else e.stderr,
            ) from None
        raise
    if text:
        return subprocess.CompletedProcess(
            r.args if r.args is not None else cmd,
            r.returncode,
            r.stdout.decode(),
            r.stderr.decode(),
        )
    return r  # type: ignore[return-value]


def _to_str(p: str | os.PathLike[str] | None) -> str | None:
    if p is None:
        return None
    return os.fspath(p)


def run_or_raise(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> str:
    return _run_or_raise(cmd, cwd=_to_str(cwd))


def run_ok(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> bool:
    return _run_ok(cmd, cwd=_to_str(cwd))


def run_quiet(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> subprocess.CompletedProcess[str]:
    return _run_quiet(cmd, cwd=_to_str(cwd))


async def run_async(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str] | subprocess.CompletedProcess[bytes]:
    return await _run_async(
        cmd, cwd=_to_str(cwd), check=check, text=text, timeout=timeout, env=env
    )


async def run_shell_async(
    cmd: str,
    *,
    cwd: str | os.PathLike[str] | None = None,
    env: dict[str, str] | None = None,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a shell command string asynchronously.

    Returns subprocess.CompletedProcess with decoded stdout and stderr.
    On timeout, returns rc=124 with a timeout message appended to stderr.
    """
    return await _run_shell_async(cmd, cwd=_to_str(cwd), env=env, timeout=timeout)


async def run_ok_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> bool:
    return await _run_ok_async(cmd, cwd=_to_str(cwd))


async def run_quiet_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> int:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
        start_new_session=True,
    )
    await proc.wait()
    assert proc.returncode is not None
    return proc.returncode


async def run_or_raise_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> str:
    r = await run_async(cmd, cwd=cwd, check=True)
    stdout = r.stdout
    if isinstance(stdout, bytes):
        return stdout.decode().strip()
    return stdout.strip()


async def iter_lines(
    stream: asyncio.StreamReader,
    *,
    idle_timeout: float | None = None,
) -> AsyncIterator[bytes]:
    """Yield newline-terminated lines from stream without a per-line size limit."""
    buf = b""
    while True:
        chunk = await asyncio.wait_for(stream.read(4096), timeout=idle_timeout)
        if not chunk:
            if buf:
                yield buf
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            yield line + b"\n"


async def terminate_with_grace(
    p: asyncio.subprocess.Process, grace_s: float = 10.0
) -> None:
    """SIGTERM → wait grace_s → SIGKILL (targets only the specific PID, not the process group)."""
    await _terminate_with_grace(p.pid, grace_s=grace_s)


async def _pump_prefixed(
    stream: asyncio.StreamReader, prefix: str, *, log_file: Any = None
) -> None:
    # Read in chunks so a child emitting a huge un-newlined blob cannot deadlock
    # by filling the pipe buffer. Re-split on newlines for the [prefix] label.
    while True:
        chunk = await stream.read(4096)
        if not chunk:
            break
        for line in chunk.decode("utf-8", "replace").splitlines(keepends=True):
            sys.stdout.write(f"[{prefix}] {line}")
            if log_file is not None:
                try:
                    log_file.write(line)
                except Exception:
                    pass
        sys.stdout.flush()


async def spawn_with_pumps(
    spec_path: pathlib.Path, attempt: str, *, log_file: Any = None
) -> tuple[asyncio.subprocess.Process, list[asyncio.Task[None]]]:
    child_proc = await asyncio.create_subprocess_exec(
        sys.executable,
        "-m",
        "gremlins.spawn.child",
        str(spec_path),
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )
    pump_out = asyncio.create_task(
        _pump_prefixed(child_proc.stdout, attempt, log_file=log_file)  # type: ignore[arg-type]
    )
    pump_err = asyncio.create_task(
        _pump_prefixed(child_proc.stderr, attempt, log_file=log_file)  # type: ignore[arg-type]
    )
    return child_proc, [pump_out, pump_err]


async def wait_child_proc(
    child_proc: asyncio.subprocess.Process,
    timeout_s: float | None,
    child_key: str,
) -> None:
    if timeout_s is None:
        await child_proc.wait()
        return
    try:
        await asyncio.wait_for(child_proc.wait(), timeout=timeout_s)
    except TimeoutError:
        await terminate_with_grace(child_proc)
        raise RuntimeError(f"parallel child {child_key!r} timed out after {timeout_s}s")
