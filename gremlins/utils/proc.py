# pyright: reportPrivateUsage=false, reportUnusedImport=false

from __future__ import annotations

import asyncio
import os
import pathlib
import subprocess
import sys
from collections.abc import AsyncIterator
from typing import Any

from gremlins._core import (  # noqa: F401
    _run_async,
    _run_ok_async,
    _run_or_raise_async,
    _run_quiet_async,
    _run_shell_async,
    _terminate_with_grace,
    run,
    run_ok,
    run_or_raise,
    run_quiet,
)


async def run_async(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return await _run_async(cmd, cwd=cwd, check=check, text=text, timeout=timeout)


async def run_shell_async(
    cmd: str,
    *,
    cwd: str | os.PathLike[str] | None = None,
    env: dict[str, str] | None = None,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return await _run_shell_async(cmd, cwd=cwd, env=env, timeout=timeout)


async def run_ok_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> bool:
    return await _run_ok_async(cmd, cwd=cwd)


async def run_quiet_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> int:
    return await _run_quiet_async(cmd, cwd=cwd)


async def run_or_raise_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> str:
    return await _run_or_raise_async(cmd, cwd=cwd)


async def terminate_with_grace(
    p: asyncio.subprocess.Process, grace_s: float = 10.0
) -> None:
    """SIGTERM → wait grace_s → SIGKILL. Shielded so it completes under cancellation.

    p must be a session leader (started with start_new_session=True).
    """
    # Signal the process group (shielded so cancellation doesn't interrupt it)
    await asyncio.shield(_terminate_with_grace(p.pid, grace_s=grace_s))
    # Reap the child through asyncio so the event loop stays consistent
    try:
        await asyncio.shield(p.wait())
    except Exception:
        pass


async def wait_child_proc(
    child_proc: asyncio.subprocess.Process,
    timeout_s: float | None,
    child_key: str,
) -> None:
    """Wait for child_proc with optional timeout. On timeout, terminate and raise."""
    if timeout_s is not None:
        try:
            await asyncio.wait_for(child_proc.wait(), timeout=timeout_s)
        except TimeoutError:
            await terminate_with_grace(child_proc)
            raise RuntimeError(
                f"parallel child {child_key!r} timed out after {timeout_s}s"
            ) from None
    else:
        await child_proc.wait()


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


async def _pump_prefixed(
    stream: asyncio.StreamReader, prefix: str, *, log_file: Any = None
) -> None:
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