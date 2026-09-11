from __future__ import annotations

import asyncio
import codecs
import logging
import os
import pathlib
import re
import subprocess
import sys
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


async def terminate_with_grace(
    p: asyncio.subprocess.Process, grace_s: float = 10.0
) -> None:
    """SIGTERM → wait grace_s → SIGKILL (targets only the specific PID, not the process group)."""
    await _terminate_with_grace(p.pid, grace_s=grace_s)


_MAX_PENDING_BYTES = 64 * 1024

# One record is everything up to and including \r\n, \r or \n; trailing text with
# no terminator is the pending remainder.
_RECORD = re.compile(r".*?(?:\r\n|\r|\n)|.+\Z", re.S)


def _emit_prefixed_line(prefix: str, line: str, log_file: Any) -> None:
    """Write one complete line to stdout and optionally *log_file*, swallowing
    errors on either path so a broken stream cannot kill the pump."""
    try:
        sys.stdout.write(f"[{prefix}] {line}")
    except Exception:
        pass
    if log_file is not None:
        try:
            log_file.write(line)
        except Exception:
            pass


def _flush_stdout() -> None:
    try:
        sys.stdout.flush()
    except Exception:
        pass


async def _pump_prefixed(
    stream: asyncio.StreamReader, prefix: str, *, log_file: Any = None
) -> None:
    # Read in chunks so a child emitting a huge un-newlined blob cannot deadlock
    # by filling the pipe buffer. Only a *trailing partial* record is carried
    # across reads, so the [prefix] label is never inserted mid-line; it is
    # relayed at EOF, or once it outgrows _MAX_PENDING_BYTES so a child that
    # writes progress without newlines stays live instead of buffering forever.
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    pending = ""
    while True:
        chunk = await stream.read(4096)
        if not chunk:
            tail = pending + decoder.decode(chunk, True)
            if tail:
                _emit_prefixed_line(prefix, tail + "\n", log_file)
            break
        records = _RECORD.findall(pending + decoder.decode(chunk))
        pending = records.pop() if records and records[-1][-1] not in "\n\r" else ""
        for record in records:
            _emit_prefixed_line(prefix, record, log_file)
        if len(pending) > _MAX_PENDING_BYTES:
            _emit_prefixed_line(prefix, pending + "\n", log_file)
            pending = ""
        _flush_stdout()


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
