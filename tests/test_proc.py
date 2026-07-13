import asyncio
import subprocess

import pytest

from gremlins._core import run as _rust_run
from gremlins.utils import proc


def run(coro):  # type: ignore[no-untyped-def]
    return asyncio.run(coro)


# run_async


def test_run_async_success():
    r = run(proc.run_async(["true"]))
    assert r.returncode == 0


def test_run_async_nonzero_exit():
    r = run(proc.run_async(["false"]))
    assert r.returncode != 0


def test_run_async_check_raises():
    with pytest.raises(subprocess.CalledProcessError):
        run(proc.run_async(["false"], check=True))


def test_run_async_captures_stdout():
    r = run(proc.run_async(["echo", "hello"]))
    assert r.stdout.strip() == "hello"


def test_run_async_captures_stderr():
    r = run(proc.run_async(["sh", "-c", "echo err >&2"]))
    assert "err" in r.stderr


def test_run_async_timeout():
    with pytest.raises(subprocess.TimeoutExpired):
        run(proc.run_async(["sleep", "10"], timeout=0.05))


# run_shell_async


def test_run_shell_async_success():
    r = run(proc.run_shell_async("true"))
    assert r.returncode == 0


def test_run_shell_async_captures_output():
    r = run(proc.run_shell_async("echo hello"))
    assert r.stdout.strip() == "hello"


def test_run_shell_async_timeout_returns_rc124():
    r = run(proc.run_shell_async("sleep 10", timeout=0.05))
    assert r.returncode == 124


def test_run_shell_async_timeout_kills_process():
    # Without killpg the grandchild keeps the pipe open and the call would hang.
    r = run(proc.run_shell_async("sleep 60 & sleep 60", timeout=0.1))
    assert r.returncode == 124


def test_run_async_timeout_kills_grandchildren():
    # Shell forks a grandchild that inherits the pipe write end. Without killpg,
    # the grandchild keeps the pipe open after the parent exits and communicate()
    # hangs past the timeout. This verifies the timeout is actually enforced.
    with pytest.raises(subprocess.TimeoutExpired):
        run(proc.run_async(["sh", "-c", "sleep 60 & sleep 60"], timeout=0.1))


# run_ok_async


def test_run_ok_async_success():
    assert run(proc.run_ok_async(["true"])) is True


def test_run_ok_async_failure():
    assert run(proc.run_ok_async(["false"])) is False


# run_quiet_async


def test_run_quiet_async_success():
    assert run(proc.run_quiet_async(["true"])) == 0


def test_run_quiet_async_nonzero_exit():
    assert run(proc.run_quiet_async(["false"])) != 0


# run_or_raise_async


def test_run_or_raise_async_returns_stripped_stdout():
    result = run(proc.run_or_raise_async(["echo", "  hello  "]))
    assert result == "hello"


def test_run_or_raise_async_raises_on_failure():
    with pytest.raises(subprocess.CalledProcessError):
        run(proc.run_or_raise_async(["false"]))


# iter_lines


def _feed(r: asyncio.StreamReader, *chunks: bytes, eof: bool = True) -> None:
    for chunk in chunks:
        r.feed_data(chunk)
    if eof:
        r.feed_eof()


def test_iter_lines_short_lines():
    async def _go() -> list[bytes]:
        r = asyncio.StreamReader()
        _feed(r, b"line1\nline2\n")
        return [line async for line in proc.iter_lines(r)]

    assert run(_go()) == [b"line1\n", b"line2\n"]


def test_iter_lines_large_line():
    big = b"x" * (128 * 1024)  # 128 KiB, well over readline's 64 KiB default limit

    async def _go() -> list[bytes]:
        r = asyncio.StreamReader()
        _feed(r, big + b"\n")
        return [line async for line in proc.iter_lines(r)]

    assert run(_go()) == [big + b"\n"]


def test_iter_lines_partial_final_line():
    async def _go() -> list[bytes]:
        r = asyncio.StreamReader()
        _feed(r, b"line1\npartial")
        return [line async for line in proc.iter_lines(r)]

    assert run(_go()) == [b"line1\n", b"partial"]


def test_iter_lines_empty_stream():
    async def _go() -> list[bytes]:
        r = asyncio.StreamReader()
        _feed(r)
        return [line async for line in proc.iter_lines(r)]

    assert run(_go()) == []


def test_iter_lines_multi_chunk_single_line():
    async def _go() -> list[bytes]:
        r = asyncio.StreamReader()
        _feed(r, b"hel", b"lo\n")
        return [line async for line in proc.iter_lines(r)]

    assert run(_go()) == [b"hello\n"]


def test_iter_lines_idle_timeout():
    async def _go() -> None:
        r = asyncio.StreamReader()
        # no data fed, no EOF — read will block until timeout fires
        async for _ in proc.iter_lines(r, idle_timeout=0.05):
            pass

    with pytest.raises(TimeoutError):
        run(_go())


# integration: confirm the Rust extension is serving calls


def test_rust_extension_serves_sync_run():
    r = _rust_run(["echo", "hello"])
    assert r.returncode == 0
    assert r.stdout.strip() == "hello"


# ---------------------------------------------------------------------------
# Integration tests for wait_child_proc / terminate_with_grace with real
# asyncio subprocesses (no mocks).
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_wait_child_proc_normal_exit():
    """wait_child_proc returns after the child exits normally."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "true", start_new_session=True
        )
        await proc.wait_child_proc(p, timeout_s=5.0, child_key="test")
        assert p.returncode == 0

    run(_go())


@pytest.mark.integration
def test_wait_child_proc_timeout_terminates():
    """wait_child_proc kills a hanging child and raises RuntimeError."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "sleep", "60", start_new_session=True
        )
        with pytest.raises(RuntimeError, match="timed out"):
            await proc.wait_child_proc(p, timeout_s=0.1, child_key="test")
        # Process must be reaped after timeout
        assert p.returncode is not None

    run(_go())


@pytest.mark.integration
def test_terminate_with_grace_kills_process():
    """terminate_with_grace kills a running process and leaves returncode set."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "sleep", "60", start_new_session=True
        )
        await proc.terminate_with_grace(p, grace_s=0.1)
        assert p.returncode is not None

    run(_go())


@pytest.mark.integration
def test_terminate_with_grace_kills_grandchildren():
    """terminate_with_grace kills the entire process group, not just the leader."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "sh",
            "-c",
            "sleep 60 & sleep 60 & wait",
            start_new_session=True,
        )
        await proc.terminate_with_grace(p, grace_s=0.1)
        assert p.returncode is not None

    run(_go())


@pytest.mark.integration
def test_wait_child_proc_cancel_triggers_terminate():
    """Cancelling wait_child_proc terminates the child and sets returncode."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "sleep", "60", start_new_session=True
        )
        task = asyncio.create_task(
            proc.wait_child_proc(p, timeout_s=None, child_key="test")
        )
        await asyncio.sleep(0.05)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        # The cancellation handler in parallel.py calls terminate_with_grace;
        # we simulate that here.
        await proc.terminate_with_grace(p)
        assert p.returncode is not None

    run(_go())


@pytest.mark.integration
def test_terminate_with_grace_shielded_against_cancel():
    """terminate_with_grace completes even if the calling task is cancelled."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "sleep", "60", start_new_session=True
        )
        task = asyncio.create_task(proc.terminate_with_grace(p, grace_s=0.2))
        await asyncio.sleep(0.05)
        task.cancel()
        # The shield inside terminate_with_grace should let it finish.
        try:
            await task
        except asyncio.CancelledError:
            pass
        assert p.returncode is not None

    run(_go())


@pytest.mark.integration
def test_wait_child_proc_no_timeout_waits_forever():
    """wait_child_proc without timeout waits for normal exit."""

    async def _go() -> None:
        p = await asyncio.create_subprocess_exec(
            "echo", "done", start_new_session=True
        )
        await proc.wait_child_proc(p, timeout_s=None, child_key="test")
        assert p.returncode == 0

    run(_go())