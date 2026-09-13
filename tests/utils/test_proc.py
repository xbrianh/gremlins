import pytest
from _gremlins_core.utils.proc import CalledProcessError, TimeoutExpired

from gremlins.utils import proc


def test_run_success():
    r = proc.run(["true"])
    assert r.returncode == 0


def test_run_failure_no_raise():
    r = proc.run(["false"])
    assert r.returncode != 0


def test_run_check_raises():
    with pytest.raises(CalledProcessError):
        proc.run(["false"], check=True)


def test_run_captures_stdout():
    r = proc.run(["echo", "hello"])
    assert r.stdout.strip() == "hello"


def test_run_captures_stderr():
    r = proc.run(["sh", "-c", "echo err >&2"])
    assert "err" in r.stderr


def test_run_ok_success():
    assert proc.run_ok(["true"]) is True


def test_run_ok_failure():
    assert proc.run_ok(["false"]) is False


def test_run_quiet_success():
    r = proc.run_quiet(["true"])
    assert r.returncode == 0


def test_run_quiet_failure():
    r = proc.run_quiet(["false"])
    assert r.returncode != 0


def test_run_quiet_no_output():
    r = proc.run_quiet(["echo", "hello"])
    assert r.stdout is None
    assert r.stderr is None


def test_run_or_raise_returns_stripped_stdout():
    result = proc.run_or_raise(["echo", "  hello  "])
    assert result == "hello"


def test_run_or_raise_raises_on_failure():
    with pytest.raises(CalledProcessError):
        proc.run_or_raise(["false"])


def test_called_process_error_output_alias():
    with pytest.raises(CalledProcessError) as exc:
        proc.run(["sh", "-c", "echo out; exit 1"], check=True)
    assert exc.value.output == exc.value.stdout == "out\n"


def test_timeout_expired_output_alias():
    with pytest.raises(TimeoutExpired) as exc:
        proc.run(["sh", "-c", "echo start; sleep 10"], timeout=0.1)
    assert exc.value.output == exc.value.stdout
    assert "start" in exc.value.output


def test_run_timeout_raises():
    with pytest.raises(TimeoutExpired) as exc:
        proc.run(["sleep", "10"], timeout=0.05)
    assert "TimeoutExpired" in exc.exconly()


def test_run_timeout_with_partial_output():
    with pytest.raises(TimeoutExpired) as exc:
        proc.run(
            ["sh", "-c", "echo start; sleep 10"],
            timeout=0.1,
        )
    assert "start" in exc.value.stdout


def test_run_timeout_large_output():
    """Output larger than pipe buffer (~64KB) under timeout must not deadlock."""
    with pytest.raises(TimeoutExpired) as exc:
        proc.run(
            ["sh", "-c", "dd if=/dev/zero bs=131072 count=1 2>/dev/null; sleep 10"],
            timeout=0.2,
        )
    assert len(exc.value.stdout) > 0


def test_run_ok_missing_command_raises_oserror():
    with pytest.raises(FileNotFoundError):
        proc.run_ok(["_nonexistent_command_xyzzy_"])
