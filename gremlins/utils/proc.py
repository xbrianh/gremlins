from __future__ import annotations

import asyncio
import os
import subprocess


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
        raise subprocess.TimeoutExpired(cmd, timeout or 0)
    rc = proc.returncode if proc.returncode is not None else -1
    stdout = stdout_b.decode()
    stderr = stderr_b.decode()
    result: subprocess.CompletedProcess[str] = subprocess.CompletedProcess(
        cmd, rc, stdout, stderr
    )
    if check and rc != 0:
        raise subprocess.CalledProcessError(rc, cmd, stdout, stderr)
    return result


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
    return proc.returncode if proc.returncode is not None else -1


async def run_or_raise_async(
    cmd: list[str], *, cwd: str | os.PathLike[str] | None = None
) -> str:
    r = await run_async(cmd, cwd=cwd, check=True)
    return r.stdout.strip()
