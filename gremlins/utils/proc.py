from __future__ import annotations

import asyncio
import os
import pathlib
import signal
import subprocess
import sys
from collections.abc import AsyncIterator
from typing import Any

from _gremlins_core.utils.proc import (
    run as _run,
)
from _gremlins_core.utils.proc import (
    run_ok,  # noqa: F401  # pyright: ignore[reportUnusedImport]
)


def run(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return _run(cmd, cwd=_to_str(cwd), check=check, timeout=timeout)


def _to_str(p: str | os.PathLike[str] | None) -> str | None:
    if p is None:
        return None
    return os.fspath(p)


def run_quiet(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> subprocess.CompletedProcess[str]:
    r = subprocess.run(
        cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    return subprocess.CompletedProcess(cmd, r.returncode)


def run_or_raise(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> str:
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=True)
    return r.stdout.strip()


async def run_async(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout_b, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        await proc.communicate()
        raise subprocess.TimeoutExpired(cmd, timeout or 0)
    except asyncio.CancelledError:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        await asyncio.shield(proc.communicate())
        raise
    assert proc.returncode is not None
    rc = proc.returncode
    stdout = stdout_b.decode() if text else stdout_b
    stderr = stderr_b.decode() if text else stderr_b
    result = subprocess.CompletedProcess(cmd, rc, stdout, stderr)
    if check and rc != 0:
        raise subprocess.CalledProcessError(rc, cmd, stdout, stderr)
    return result  # type: ignore[return-value]


async def run_shell_async(
    cmd: str,
    *,
    cwd: str | os.PathLike[str] | None = None,
    env: dict[str, str] | None = None,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    proc = await asyncio.create_subprocess_shell(
        cmd,
        cwd=cwd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
        start_new_session=True,
    )
    try:
        stdout_b, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        stdout_b, stderr_b = await proc.communicate()
        return subprocess.CompletedProcess(
            cmd,
            124,
            stdout_b.decode(),
            stderr_b.decode() + f"timed out after {timeout}s\n",
        )
    except asyncio.CancelledError:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        await asyncio.shield(proc.communicate())
        raise
    assert proc.returncode is not None
    return subprocess.CompletedProcess(
        cmd, proc.returncode, stdout_b.decode(), stderr_b.decode()
    )


async def run_ok_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> bool:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
        start_new_session=True,
    )
    await proc.wait()
    return proc.returncode == 0


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
    return r.stdout.strip()


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
    """SIGTERM → wait grace_s → SIGKILL. Shielded so it completes under cancellation.

    p must be a session leader (started with start_new_session=True).
    """
    try:
        os.killpg(p.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        return
    cancelled = False
    try:
        await asyncio.shield(asyncio.wait_for(p.wait(), timeout=grace_s))
    except asyncio.CancelledError:
        cancelled = True
    except TimeoutError:
        pass
    if p.returncode is None:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        await asyncio.shield(p.wait())
    if cancelled:
        raise asyncio.CancelledError()


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
