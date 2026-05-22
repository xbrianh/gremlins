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


def test_subprocess_client_bypass_true_uses_bypass_permissions(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    argv_out = tmp_path / "child_argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessClaudeClient(bypass=True)
    asyncio.run(client.run("hello", label="test"))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    idx = argv.index("--permission-mode")
    assert argv[idx + 1] == "bypassPermissions"


def test_subprocess_client_bypass_false_uses_default_mode(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    argv_out = tmp_path / "child_argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessClaudeClient(bypass=False)
    asyncio.run(client.run("hello", label="test"))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--permission-mode" in argv
    idx = argv.index("--permission-mode")
    assert argv[idx + 1] == "default"
    assert "bypassPermissions" not in argv


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
    asyncio.run(
        client.run(
            "original", label="test", on_timeout_prompt="retry-prompt", max_retries=2
        )
    )
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
        asyncio.run(
            client.run("hello", label="test", max_retries=len(STREAM_IDLE_BACKOFF) + 1)
        )


# ---------------------------------------------------------------------------
# Config materialization / CLAUDE_CONFIG_DIR tests
# ---------------------------------------------------------------------------


def test_non_empty_block_writes_settings_and_sets_claude_config_dir(
    tmp_path, monkeypatch
):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    env_out = tmp_path / "child_env.json"
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))

    block = {"allowedTools": ["Read", "Edit", "Bash", "Write", "Grep", "Glob"]}
    client = SubprocessClaudeClient(bypass=False, native_block=block)
    asyncio.run(
        client.run(
            "hello", label="test", extra_env={"GREMLIN_STATE_DIR": str(state_dir)}
        )
    )

    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    config_dir = pathlib.Path(child_env["CLAUDE_CONFIG_DIR"])
    assert config_dir == state_dir / "claude-config"
    settings = json.loads(
        (config_dir / ".claude" / "settings.json").read_text(encoding="utf-8")
    )
    assert settings == block


def test_bypass_true_with_block_still_sets_claude_config_dir(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    env_out = tmp_path / "child_env.json"
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))

    block = {"allowedTools": ["Read"]}
    client = SubprocessClaudeClient(bypass=True, native_block=block)
    asyncio.run(
        client.run(
            "hello", label="test", extra_env={"GREMLIN_STATE_DIR": str(state_dir)}
        )
    )

    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    assert "CLAUDE_CONFIG_DIR" in child_env
    settings_path = state_dir / "claude-config" / ".claude" / "settings.json"
    assert settings_path.exists()


def test_empty_block_no_claude_config_dir(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    env_out = tmp_path / "child_env.json"
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))
    monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)

    client = SubprocessClaudeClient()
    asyncio.run(
        client.run(
            "hello", label="test", extra_env={"GREMLIN_STATE_DIR": str(state_dir)}
        )
    )

    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    assert "CLAUDE_CONFIG_DIR" not in child_env


def test_credentials_symlinked_on_non_darwin(tmp_path, monkeypatch):
    fake_home = tmp_path / "home"
    creds_src = fake_home / ".claude" / ".credentials.json"
    creds_src.parent.mkdir(parents=True)
    creds_src.write_text('{"token": "test"}', encoding="utf-8")

    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setattr(pathlib.Path, "home", classmethod(lambda cls: fake_home))

    state_dir = tmp_path / "state"
    client = SubprocessClaudeClient(native_block={"allowedTools": ["Read"]})
    config_dir = client._materialize_config(state_dir)

    creds_dst = config_dir / ".claude" / ".credentials.json"
    assert creds_dst.is_symlink()
    assert creds_dst.resolve() == creds_src


def test_no_credentials_symlink_on_darwin(tmp_path, monkeypatch):
    fake_home = tmp_path / "home"
    creds_src = fake_home / ".claude" / ".credentials.json"
    creds_src.parent.mkdir(parents=True)
    creds_src.write_text('{"token": "test"}', encoding="utf-8")

    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(pathlib.Path, "home", classmethod(lambda cls: fake_home))

    state_dir = tmp_path / "state"
    client = SubprocessClaudeClient(native_block={"allowedTools": ["Read"]})
    config_dir = client._materialize_config(state_dir)

    creds_dst = config_dir / ".claude" / ".credentials.json"
    assert not creds_dst.exists()


def test_native_block_without_state_dir_raises(monkeypatch):
    monkeypatch.delenv("GREMLIN_STATE_DIR", raising=False)
    client = SubprocessClaudeClient(native_block={"allowedTools": ["Read"]})
    with pytest.raises(RuntimeError, match="GREMLIN_STATE_DIR absent"):
        asyncio.run(client.run("hello", label="test"))


# ---------------------------------------------------------------------------
# _to_claude_settings translation
# ---------------------------------------------------------------------------


def test_allowed_tools_translated_to_permissions_allow(tmp_path):
    from gremlins.clients.claude import _to_claude_settings

    block = {"allowed_tools": ["Read", "Edit", "Bash"]}
    assert _to_claude_settings(block) == {"permissions": {"allow": ["Read", "Edit", "Bash"]}}


def test_disallowed_tools_translated_to_permissions_deny(tmp_path):
    from gremlins.clients.claude import _to_claude_settings

    block = {"disallowed_tools": ["Bash"]}
    assert _to_claude_settings(block) == {"permissions": {"deny": ["Bash"]}}


def test_both_translated_together():
    from gremlins.clients.claude import _to_claude_settings

    block = {"allowed_tools": ["Read"], "disallowed_tools": ["Bash"]}
    assert _to_claude_settings(block) == {"permissions": {"allow": ["Read"], "deny": ["Bash"]}}


def test_native_keys_pass_through():
    from gremlins.clients.claude import _to_claude_settings

    block = {"theme": "dark", "allowedTools": ["Read"]}
    assert _to_claude_settings(block) == {"theme": "dark", "allowedTools": ["Read"]}


def test_empty_block_stays_empty():
    from gremlins.clients.claude import _to_claude_settings

    assert _to_claude_settings({}) == {}


# ---------------------------------------------------------------------------
# Task 3: regression — default block materializes to permissions.allow
# ---------------------------------------------------------------------------


def test_default_block_materializes_permissions_allow(tmp_path):
    """Default claude block must produce permissions.allow, not be silently dropped."""
    from gremlins.permissions.loader import load_default_block

    default_block = load_default_block("claude")
    client = SubprocessClaudeClient(bypass=False, native_block=default_block)
    config_dir = client._materialize_config(tmp_path / "state")
    settings = json.loads(
        (config_dir / ".claude" / "settings.json").read_text(encoding="utf-8")
    )
    assert "permissions" in settings, "settings.json missing 'permissions' key"
    assert "allow" in settings["permissions"], "permissions missing 'allow'"
    allowed = set(settings["permissions"]["allow"])
    assert {"Read", "Edit", "Bash", "Write", "Grep", "Glob"} <= allowed


# ---------------------------------------------------------------------------
# Task 4: end-to-end guard — default mode run with bundled defaults
# ---------------------------------------------------------------------------


def test_default_mode_with_bundled_defaults_sets_permissions_allow(tmp_path, monkeypatch):
    """E2E guard: non-bypass run using bundled claude defaults sets CLAUDE_CONFIG_DIR
    with a usable permissions.allow that Claude CLI can honour."""
    bin_dir = tmp_path / "bin"
    _install_stub_claude(bin_dir)
    env_out = tmp_path / "child_env.json"
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))

    from gremlins.permissions.loader import load_default_block

    default_block = load_default_block("claude")
    client = SubprocessClaudeClient(bypass=False, native_block=default_block)
    asyncio.run(
        client.run("hello", label="test", extra_env={"GREMLIN_STATE_DIR": str(state_dir)})
    )

    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    config_dir = pathlib.Path(child_env["CLAUDE_CONFIG_DIR"])
    settings = json.loads(
        (config_dir / ".claude" / "settings.json").read_text(encoding="utf-8")
    )
    assert "permissions" in settings
    allowed = set(settings["permissions"].get("allow", []))
    assert {"Read", "Edit", "Bash", "Write", "Grep", "Glob"} <= allowed
