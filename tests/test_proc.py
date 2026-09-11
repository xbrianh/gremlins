import asyncio
import io
import subprocess
import sys

import pytest

from gremlins.utils import proc


def run(coro):  # type: ignore[no-untyped-def]
    return asyncio.run(coro)


class _ChunkedReader:
    """Yields preset chunks regardless of the requested size."""

    def __init__(self, chunks: list[bytes]) -> None:
        self._chunks = chunks

    async def read(self, n: int) -> bytes:
        return self._chunks.pop(0) if self._chunks else b""


def pump(chunks: list[bytes], prefix: str = "p", **kwargs):  # type: ignore[no-untyped-def]
    return run(proc._pump_prefixed(_ChunkedReader(chunks), prefix, **kwargs))


# _pump_prefixed


def test_pump_label_survives_chunk_boundary(capsys):  # type: ignore[no-untyped-def]
    pump([b"par", b"tial\nnext\n"])
    assert capsys.readouterr().out == "[p] partial\n[p] next\n"


def test_pump_flushes_trailing_partial_line_at_eof(capsys):  # type: ignore[no-untyped-def]
    pump([b"no newline"])
    assert capsys.readouterr().out == "[p] no newline\n"


def test_pump_splits_records_on_bare_cr(capsys):  # type: ignore[no-untyped-def]
    pump([b"one\rtwo\r\n"])
    assert capsys.readouterr().out == "[p] one\r[p] two\r\n"


def test_pump_holds_terminal_cr_until_next_record(capsys):  # type: ignore[no-untyped-def]
    pump([b"one\r", b"\ntwo\n"])
    assert capsys.readouterr().out == "[p] one\r\n[p] two\n"


def test_pump_flushes_bare_cr_at_eof(capsys):  # type: ignore[no-untyped-def]
    pump([b"one\r"])
    assert capsys.readouterr().out == "[p] one\r"


def test_pump_joins_multibyte_sequence_across_chunks(capsys):  # type: ignore[no-untyped-def]
    pump([b"caf\xc3", b"\xa9\n"])
    assert capsys.readouterr().out == "[p] caf\u00e9\n"


def test_pump_flushes_oversized_partial_line_before_eof(capsys):  # type: ignore[no-untyped-def]
    blob = b"x" * (proc._MAX_PENDING_BYTES + 10)
    emitted: list[str] = []

    class _Reader:
        def __init__(self) -> None:
            self._chunks = [blob]

        async def read(self, n: int) -> bytes:
            if not self._chunks:
                emitted.append(capsys.readouterr().out)
                return b""
            return self._chunks.pop(0)

    run(proc._pump_prefixed(_Reader(), "p"))
    assert emitted == [f"[p] {blob.decode()}\n"]


def test_pump_survives_broken_stdout(monkeypatch):  # type: ignore[no-untyped-def]
    log = io.StringIO()

    class _Boom:
        def write(self, _: str) -> None:
            raise OSError("broken pipe")

        def flush(self) -> None:
            raise OSError("broken pipe")

    monkeypatch.setattr(sys, "stdout", _Boom())
    pump([b"a\nb\n"], log_file=log)
    assert log.getvalue() == "a\nb\n"


def test_pump_survives_broken_log_file(capsys):  # type: ignore[no-untyped-def]
    class _Boom:
        def write(self, _: str) -> None:
            raise ValueError("closed file")

    pump([b"a\nb\n"], log_file=_Boom())
    assert capsys.readouterr().out == "[p] a\n[p] b\n"


def test_pump_flushes_each_record(monkeypatch):  # type: ignore[no-untyped-def]
    flushes: list[str] = []

    class _Spy:
        def write(self, text: str) -> None:
            flushes.append(f"w:{text}")

        def flush(self) -> None:
            flushes.append("flush")

    monkeypatch.setattr(sys, "stdout", _Spy())
    pump([b"line\n", b"tail"])
    assert flushes == ["w:[p] line\n", "flush", "w:[p] tail\n", "flush"]


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
