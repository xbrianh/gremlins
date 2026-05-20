import asyncio
import subprocess

import pytest

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


def test_iter_lines_empty():
    async def t():
        r = asyncio.StreamReader()
        r.feed_eof()
        assert [line async for line in proc.iter_lines(r)] == []
    run(t())


def test_iter_lines_with_newlines():
    async def t():
        r = asyncio.StreamReader()
        r.feed_data(b"hello\nworld\n")
        r.feed_eof()
        assert [line async for line in proc.iter_lines(r)] == [b"hello\n", b"world\n"]
    run(t())


def test_iter_lines_without_final_newline():
    async def t():
        r = asyncio.StreamReader()
        r.feed_data(b"hello\nworld")
        r.feed_eof()
        assert [line async for line in proc.iter_lines(r)] == [b"hello\n", b"world"]
    run(t())


def test_iter_lines_idle_timeout():
    async def t():
        r = asyncio.StreamReader()
        with pytest.raises(TimeoutError):
            async for _ in proc.iter_lines(r, idle_timeout=0.01):
                pass
    run(t())


def test_iter_lines_long_line():
    async def t():
        r = asyncio.StreamReader()
        data = b"x" * 100000
        r.feed_data(data)
        r.feed_eof()
        assert [line async for line in proc.iter_lines(r)] == [data]
    run(t())
