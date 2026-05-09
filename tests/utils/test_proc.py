import subprocess

import pytest

from gremlins.utils import proc


def test_run_success():
    r = proc.run(["true"])
    assert r.returncode == 0


def test_run_failure_no_raise():
    r = proc.run(["false"])
    assert r.returncode != 0


def test_run_check_raises():
    with pytest.raises(subprocess.CalledProcessError):
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
    with pytest.raises(subprocess.CalledProcessError):
        proc.run_or_raise(["false"])
