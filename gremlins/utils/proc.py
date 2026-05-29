from __future__ import annotations

import asyncio
import json
import os
import pathlib
import signal
import subprocess
import sys
from collections.abc import AsyncIterator, Callable
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from gremlins.executor.state import State
    from gremlins.stages.base import Stage


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
            cmd, 124, stdout_b.decode(), stderr_b.decode() + f"timed out after {timeout}s\n"
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
    except ProcessLookupError:
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


# ---------------------------------------------------------------------------
# Parallel child subprocess helpers
# ---------------------------------------------------------------------------


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


def _parse_child_timeout(stage_obj: Stage, child_key: str) -> float | None:
    if not stage_obj.raw_dict:
        return None
    raw_t = stage_obj.raw_dict.get("timeout_seconds")
    if raw_t is None:
        return None
    try:
        parsed_t = float(raw_t)
    except (TypeError, ValueError) as exc:
        raise ValueError(
            f"parallel child {child_key!r}: 'timeout_seconds' must be a number, "
            f"got {raw_t!r}"
        ) from exc
    return parsed_t if parsed_t > 0 else None


def _missing_result_detail(child_key: str, returncode: int | None) -> str:
    if returncode is None:
        return (
            f"parallel child {child_key!r}: subprocess exited with no result file "
            f"(returncode unavailable)"
        )
    if returncode == 0:
        return f"parallel child {child_key!r} exited 0 without writing result"
    if returncode < 0:
        try:
            sig_name = signal.Signals(-returncode).name
        except ValueError:
            sig_name = f"signal {-returncode}"
        return (
            f"parallel child {child_key!r} terminated by {sig_name} with no result file"
        )
    return f"parallel child {child_key!r} exited with returncode {returncode} and no result file"


def _build_child_spec_dict(
    stage_obj: Stage,
    child_st: State,
    child_key: str,
    attempt: str,
    group_name: str = "",
    child_id: str = "",
) -> dict[str, Any]:
    parent_id = child_st.data.gremlin_id or ""
    return {
        "stage_dict": stage_obj.raw_dict,
        "client": str(child_st.client),
        "child_id": child_id,
        "parent_id": parent_id,
        "group_name": group_name,
        "worktree": str(child_st.worktree) if child_st.worktree else None,
        "worktree_parent": (
            str(child_st.worktree_parent) if child_st.worktree_parent else None
        ),
        "pipeline_path": child_st.data.pipeline_path or None,
        "child_key": child_key,
        "attempt": attempt,
        "parent_stage": child_st.parent_stage,
        "repo": child_st.repo,
        "instructions": child_st.instructions,
        "test_client": str(child_st.test_client) if child_st.test_client else None,
        "stage_model": child_st.stage_model,
    }


async def _spawn_child_with_pumps(
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


async def _wait_child_proc(
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


def _read_child_result(
    spec_path: pathlib.Path,
    child_proc: asyncio.subprocess.Process,
    child_key: str,
) -> dict[str, Any]:
    result_path = pathlib.Path(str(spec_path) + ".result")
    if not result_path.exists():
        raise RuntimeError(_missing_result_detail(child_key, child_proc.returncode))
    return json.loads(result_path.read_text(encoding="utf-8"))


async def run_child_subprocess(
    stage_obj: Stage,
    child_st: State,
    child_key: str,
    attempt: str,
    *,
    on_bail: Callable[[str], None],
    group_name: str = "",
    child_id: str = "",
) -> tuple[str, float]:
    """Spawn gremlins.spawn.child for one parallel child; return (status, cost_usd).

    status is "done" or "bail". Raises RuntimeError on timeout, missing result,
    or error status. Propagates CancelledError after cleaning up the child process.
    on_bail is called with the bail detail string when status == "bail".
    """
    spec_path = child_st.session_dir / f"spec_{attempt}.json"
    spec_path.write_text(
        json.dumps(
            _build_child_spec_dict(
                stage_obj, child_st, child_key, attempt, group_name, child_id
            )
        ),
        encoding="utf-8",
    )
    timeout_s = _parse_child_timeout(stage_obj, child_key)
    log_path = child_st.session_dir.parent / "log"
    log_file = None
    if log_path.parent.exists():
        try:
            log_file = log_path.open("a", buffering=1, encoding="utf-8")
        except OSError:
            pass
    try:
        child_proc, pumps = await _spawn_child_with_pumps(
            spec_path, attempt, log_file=log_file
        )
        try:
            await _wait_child_proc(child_proc, timeout_s, child_key)
        except asyncio.CancelledError:
            await terminate_with_grace(child_proc)
            for p in pumps:
                p.cancel()
            raise
        finally:
            await asyncio.shield(asyncio.gather(*pumps, return_exceptions=True))
    finally:
        if log_file is not None:
            try:
                log_file.close()
            except OSError:
                pass
    result = _read_child_result(spec_path, child_proc, child_key)
    try:
        cost = float(result.get("cost_usd") or 0.0)
    except (ValueError, TypeError):
        cost = 0.0
    status = result.get("status")
    if status in ("done", "needs_fix"):
        return "done", cost
    if status == "bail":
        on_bail(result.get("detail") or "")
        return "bail", cost
    raise RuntimeError(
        f"parallel child {child_key!r} error: {result.get('detail') or ''}"
    )
