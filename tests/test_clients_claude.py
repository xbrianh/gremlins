"""Tests for SubprocessClaudeClient (gremlins/clients/claude.py)."""

from __future__ import annotations

import json
import os
import pathlib
import shlex
import sys

from gremlins.clients.claude import SubprocessClaudeClient

TESTS_DIR = pathlib.Path(__file__).resolve().parent

# ---------------------------------------------------------------------------
# Stub claude binary
# ---------------------------------------------------------------------------

_STUB_CLAUDE_SRC = """\
#!/usr/bin/env python3
import json, os, sys

env_out = os.environ.get("STUB_ENV_OUT")
if env_out:
    with open(env_out, "w", encoding="utf-8") as f:
        json.dump(dict(os.environ), f)

sys.stdout.write(json.dumps({"type": "system", "subtype": "init", "session_id": "stub-sess"}) + "\\n")
sys.stdout.write(json.dumps({"type": "result", "subtype": "success", "num_turns": 1, "total_cost_usd": 0.0}) + "\\n")
sys.stdout.flush()
"""


def _install_stub_claude(bin_dir: pathlib.Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    stub_py = bin_dir / "_stub_claude.py"
    stub_py.write_text(_STUB_CLAUDE_SRC, encoding="utf-8")
    stub_py.chmod(0o755)
    wrapper = bin_dir / "claude"
    wrapper.write_text(
        f"#!/usr/bin/env bash\nexec {shlex.quote(sys.executable)} "
        f"{shlex.quote(str(stub_py))} \"$@\"\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o755)


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
    client.run("hello", label="test", output_format="stream-json")

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
    client.run("hello", label="test", output_format="stream-json")

    child_env = json.loads(env_out.read_text(encoding="utf-8"))
    assert child_env.get("MY_SENTINEL_VAR") == sentinel
