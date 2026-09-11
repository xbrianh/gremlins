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


def test_run_shell_async_large_stdout_burst():
    # Generate > pipe buffer (typically 16KB) to verify the drain loop prevents
    # deadlock. The old communicate()-based code would hang here.
    size = 128 * 1024  # 128KB
    r = run(
        proc.run_shell_async(
            f"python3 -c \"import sys; sys.stdout.write('x' * {size}); sys.stdout.flush()\""
        )
    )
    assert r.returncode == 0
    assert len(r.stdout) == size


def test_run_shell_async_large_stderr_burst():
    # Generate large stderr to exercise the stderr drain path.
    size = 128 * 1024
    r = run(
        proc.run_shell_async(
            f"python3 -c \"import sys; sys.stderr.write('x' * {size}); sys.stderr.flush()\""
        )
    )
    assert r.returncode == 0
    assert len(r.stderr) == size


def test_run_shell_async_large_both_streams():
    # Simultaneous large output on both streams.
    size = 64 * 1024  # 64KB each = 128KB total
    r = run(
        proc.run_shell_async(
            f"python3 -c \"import sys; sys.stdout.write('x' * {size}); sys.stderr.write('y' * {size}); sys.stdout.flush(); sys.stderr.flush()\""
        )
    )
    assert r.returncode == 0
    assert len(r.stdout) == size
    assert len(r.stderr) == size


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
