from __future__ import annotations

import asyncio
import os
import subprocess
from collections.abc import AsyncIterator


def run(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd, cwd=cwd, capture_output=True, text=text, check=check, timeout=timeout
    )


def run_ok(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> bool:
    r = subprocess.run(
        cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    return r.returncode == 0


def run_quiet(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> int:
    r = subprocess.run(
        cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    return r.returncode


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
    )
    try:
        stdout_b, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
        proc.kill()
        await proc.communicate()
        raise subprocess.TimeoutExpired(cmd, timeout or 0)
    assert proc.returncode is not None
    rc = proc.returncode
    stdout = stdout_b.decode() if text else stdout_b
    stderr = stderr_b.decode() if text else stderr_b
    result = subprocess.CompletedProcess(cmd, rc, stdout, stderr)
    if check and rc != 0:
        raise subprocess.CalledProcessError(rc, cmd, stdout, stderr)
    return result  # type: ignore[return-value]


async def run_ok_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> bool:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
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
    chunk_size: int = 4096,
    idle_timeout: float | None = None,
) -> AsyncIterator[bytes]:
    """Yield newline-terminated lines from stream without a per-line size limit.

    Raises TimeoutError if a chunk doesn't arrive within idle_timeout.
    Final partial line (no trailing newline at EOF) is yielded as-is.
    """
    buf = b""
    while True:
        chunk = await asyncio.wait_for(stream.read(chunk_size), timeout=idle_timeout)
        if not chunk:
            if buf:
                yield buf
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            yield line + b"\n"
