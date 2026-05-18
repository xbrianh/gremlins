"""Tests for SubprocessClaudeClient (gremlins/clients/claude.py)."""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys

import pytest

from gremlins.clients.claude import StreamTimeoutError, SubprocessClaudeClient
from gremlins.clients.config import STREAM_IDLE_BACKOFF

TESTS_DIR = pathlib.Path(__file__).resolve().parent

# ---------------------------------------------------------------------------
# Stub claude binary
# ---------------------------------------------------------------------------

_STUB_CLAUDE_SRC = """\
import json, os, sys

env_out = os.environ.get("STUB_ENV_OUT")
if env_out:
    with open(env_out, "w", encoding="utf-8") as f:
        json.dump(dict(os.environ), f)

stdin_out = os.environ.get("STUB_STDIN_OUT")
if stdin_out:
    with open(stdin_out, "w", encoding="utf-8") as f:
        f.write(sys.stdin.read())

argv_out = os.environ.get("STUB_ARGV_OUT")
if argv_out:
    with open(argv_out, "w", encoding="utf-8") as f:
        json.dump(sys.argv[1:], f)

sys.stdout.write(json.dumps({"type": "system", "subtype": "init"}) + "\\n")
sys.stdout.write(json.dumps({"type": "result", "subtype": "success", "num_turns": 1, "total_cost_usd": 0.0}) + "\\n")
sys.stdout.flush()
"""


def _install_stub_claude(bin_dir: pathlib.Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    stub = bin_dir / "claude"
    stub.write_text(f"#!{sys.executable}\n" + _STUB_CLAUDE_SRC, encoding="utf-8")
    stub.chmod(0o755)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_subprocess_client_sets_gremlin_skip_summary(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    env_out = tmp_path / "child_env.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))
    monkeypatch.delenv("GREMLIN_SKIP_SUMMARY", raising=False)

    client = SubprocessClaudeClient()
    asyncio.run(client.run("hello", label="test"))

    assert env_out.exists(), "stub did not write env file"
    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    assert child_env.get("GREMLIN_SKIP_SUMMARY") == "1"


def test_subprocess_client_inherits_other_env_vars(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    env_out = tmp_path / "child_env.json"

    sentinel = "test_value_xyz_123"
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))
    monkeypatch.setenv("MY_SENTINEL_VAR", sentinel)

    client = SubprocessClaudeClient()
    asyncio.run(client.run("hello", label="test"))

    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    assert child_env.get("MY_SENTINEL_VAR") == sentinel


def test_subprocess_client_sends_prompt_via_stdin_not_argv(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    stdin_out = tmp_path / "child_stdin.txt"
    argv_out = tmp_path / "child_argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_STDIN_OUT", str(stdin_out))
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    prompt = "the prompt text"
    client = SubprocessClaudeClient()
    asyncio.run(client.run(prompt, label="test"))

    assert stdin_out.read_text(encoding="utf-8") == prompt
    child_argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert prompt not in child_argv


# ---------------------------------------------------------------------------
# Timeout / retry tests
# ---------------------------------------------------------------------------

_TIMEOUT_STUB_SRC = """\
import json, os, sys

count_file = os.environ.get("STUB_COUNT_FILE")
fail_times = int(os.environ.get("STUB_FAIL_TIMES", "0"))

count = 0
if count_file:
    try:
        count = int(open(count_file).read().strip())
    except Exception:
        pass
    with open(count_file, "w") as f:
        f.write(str(count + 1))

if count < fail_times:
    sys.stdout.write("API Error: Stream idle timeout\\n")
    sys.stdout.flush()
    sys.exit(1)

sys.stdout.write(json.dumps({"type": "system", "subtype": "init"}) + "\\n")
sys.stdout.write(json.dumps({"type": "result", "subtype": "success", "num_turns": 1, "total_cost_usd": 0.0}) + "\\n")
sys.stdout.flush()
"""


def _install_timeout_stub(bin_dir: pathlib.Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    stub = bin_dir / "claude"
    stub.write_text(f"#!{sys.executable}\n" + _TIMEOUT_STUB_SRC, encoding="utf-8")
    stub.chmod(0o755)


def test_retry_succeeds_on_second_attempt(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_timeout_stub(bin_dir)
    count_file = tmp_path / "count.txt"
    count_file.write_text("0")

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_COUNT_FILE", str(count_file))
    monkeypatch.setenv("STUB_FAIL_TIMES", "1")

    async def _noop_sleep(_: float) -> None:
        pass

    monkeypatch.setattr("asyncio.sleep", _noop_sleep)

    client = SubprocessClaudeClient()
    result = asyncio.run(client.run("hello", label="test", max_retries=2))
    assert result.exit_code == 0
    assert int(count_file.read_text()) == 2  # called twice


def test_retry_exhaustion_raises_stream_timeout_error(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_timeout_stub(bin_dir)
    count_file = tmp_path / "count.txt"
    count_file.write_text("0")

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_COUNT_FILE", str(count_file))
    monkeypatch.setenv("STUB_FAIL_TIMES", "99")

    async def _noop_sleep(_: float) -> None:
        pass

    monkeypatch.setattr("asyncio.sleep", _noop_sleep)

    client = SubprocessClaudeClient()
    with pytest.raises(StreamTimeoutError):
        asyncio.run(client.run("hello", label="test", max_retries=2))
    assert int(count_file.read_text()) == 3  # initial + 2 retries


_SLEEP_FOREVER_STUB_SRC = """\
import json, sys, time

sys.stdout.write(json.dumps({"type": "assistant", "message": {"content": [], "stop_reason": None}}) + "\\n")
sys.stdout.flush()
time.sleep(9999)
"""


def _install_sleep_forever_stub(bin_dir: pathlib.Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    stub = bin_dir / "claude"
    stub.write_text(f"#!{sys.executable}\n" + _SLEEP_FOREVER_STUB_SRC, encoding="utf-8")
    stub.chmod(0o755)


def test_idle_timeout_raises_stream_timeout_error(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_sleep_forever_stub(bin_dir)

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")

    client = SubprocessClaudeClient()
    with pytest.raises(StreamTimeoutError):
        asyncio.run(client.run("hello", label="test", idle_timeout=0.1, max_retries=0))


def test_on_timeout_prompt_used_on_retry(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_timeout_stub(bin_dir)
    count_file = tmp_path / "count.txt"
    count_file.write_text("0")
    stdin_out = tmp_path / "last_stdin.txt"

    stub = bin_dir / "claude"
    extra = """
stdin_out = os.environ.get("STUB_STDIN_OUT")
if stdin_out:
    with open(stdin_out, "w") as f:
        f.write(sys.stdin.read())
"""
    # Prepend stdin capture before the timeout logic
    src = _TIMEOUT_STUB_SRC.replace(
        "import json, os, sys\n",
        "import json, os, sys\n" + extra,
    )
    stub.write_text(f"#!{sys.executable}\n" + src, encoding="utf-8")
    stub.chmod(0o755)

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_COUNT_FILE", str(count_file))
    monkeypatch.setenv("STUB_FAIL_TIMES", "1")
    monkeypatch.setenv("STUB_STDIN_OUT", str(stdin_out))

    async def _noop_sleep(_: float) -> None:
        pass

    monkeypatch.setattr("asyncio.sleep", _noop_sleep)

    client = SubprocessClaudeClient()
    asyncio.run(client.run(
        "original", label="test", on_timeout_prompt="retry-prompt", max_retries=2
    ))
    assert stdin_out.read_text(encoding="utf-8") == "retry-prompt"


def test_backoff_schedule_matches_stream_idle_backoff(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_timeout_stub(bin_dir)
    count_file = tmp_path / "count.txt"
    count_file.write_text("0")

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_COUNT_FILE", str(count_file))
    monkeypatch.setenv("STUB_FAIL_TIMES", "99")

    sleep_calls: list[float] = []

    async def _record_sleep(t: float) -> None:
        sleep_calls.append(t)

    monkeypatch.setattr("asyncio.sleep", _record_sleep)

    client = SubprocessClaudeClient()
    with pytest.raises(StreamTimeoutError):
        asyncio.run(client.run("hello", label="test", max_retries=3))

    assert sleep_calls == list(STREAM_IDLE_BACKOFF)


def test_max_retries_exceeds_schedule_raises_value_error():
    client = SubprocessClaudeClient()
    with pytest.raises(ValueError, match="max_retries"):
        asyncio.run(client.run("hello", label="test", max_retries=len(STREAM_IDLE_BACKOFF) + 1))
