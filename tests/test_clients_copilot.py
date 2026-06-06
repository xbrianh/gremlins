"""Tests for SubprocessCopilotClient (gremlins/clients/copilot.py)."""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys

import pytest
from conftest import _TestGremlin

from gremlins.clients.copilot import SubprocessCopilotClient, _strip_footer

TESTS_DIR = pathlib.Path(__file__).resolve().parent

# ---------------------------------------------------------------------------
# Stub copilot binary
# ---------------------------------------------------------------------------

_STUB_COPILOT_SRC = """\
import os, sys

argv_out = os.environ.get("STUB_ARGV_OUT")
if argv_out:
    import json
    with open(argv_out, "w") as f:
        json.dump(sys.argv[1:], f)

env_out = os.environ.get("STUB_ENV_OUT")
if env_out:
    import json
    with open(env_out, "w") as f:
        json.dump(dict(os.environ), f)

stdin_out = os.environ.get("STUB_STDIN_OUT")
if stdin_out:
    with open(stdin_out, "w") as f:
        f.write(sys.stdin.read())

sys.stdout.write(os.environ.get("STUB_OUTPUT", "hello from copilot"))
sys.stdout.flush()
"""

_FAIL_STUB_SRC = """\
import sys
sys.exit(int(__import__("os").environ.get("STUB_EXIT_CODE", "1")))
"""


def _install_stub(bin_dir: pathlib.Path, src: str, name: str = "copilot") -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    stub = bin_dir / name
    stub.write_text(f"#!{sys.executable}\n" + src, encoding="utf-8")
    stub.chmod(0o755)


# ---------------------------------------------------------------------------
# _strip_footer
# ---------------------------------------------------------------------------


def test_strip_footer_removes_stats_block() -> None:
    text = "This is the response\n\n⏺ Cost: $0.01 | Duration: 3.2s | Tokens: 100"
    assert _strip_footer(text) == "This is the response"


def test_strip_footer_noop_without_footer() -> None:
    text = "Just a plain response"
    assert _strip_footer(text) == text


def test_strip_footer_multiline_footer() -> None:
    text = "Answer here\n⏺ Cost: $0.00 | Duration: 1s\nmore stats"
    assert _strip_footer(text) == "Answer here"


# ---------------------------------------------------------------------------
# SubprocessCopilotClient
# ---------------------------------------------------------------------------


def test_copilot_client_runs_and_returns_text(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_OUTPUT", "copilot response text")

    client = SubprocessCopilotClient()
    result = asyncio.run(client.run(_TestGremlin("my prompt", label="test")))

    assert result.exit_code == 0
    assert result.text_result == "copilot response text"
    assert result.events is None
    assert result.cost_usd is None


def test_copilot_client_sends_prompt_via_argv(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient()
    asyncio.run(client.run(_TestGremlin("the prompt", label="test")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    p_idx = argv.index("-p")
    assert argv[p_idx + 1] == "the prompt"


def test_copilot_client_passes_allow_all_when_bypass(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient(bypass=True)
    asyncio.run(client.run(_TestGremlin("the-prompt", label="test")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--allow-all" in argv
    assert "--allow-all-tools" not in argv
    assert argv[-1] == "the-prompt"
    assert argv[-2] == "-p"


def test_copilot_client_no_allow_all_when_not_bypass(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient(bypass=False)
    asyncio.run(client.run(_TestGremlin("the-prompt", label="test")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--allow-all" not in argv


def test_copilot_client_passes_model_when_specified(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient()
    asyncio.run(client.run(_TestGremlin("prompt", label="test", model="gpt-5.4")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--model" in argv
    assert "gpt-5.4" in argv


def test_copilot_client_omits_model_when_none(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient()
    asyncio.run(client.run(_TestGremlin("prompt", label="test", model=None)))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--model" not in argv


def test_copilot_client_raises_on_nonzero_exit(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _FAIL_STUB_SRC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_EXIT_CODE", "2")

    client = SubprocessCopilotClient()
    with pytest.raises(RuntimeError, match="copilot -p.*exited 2"):
        asyncio.run(client.run(_TestGremlin("prompt", label="test")))


def test_copilot_client_strips_footer(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_OUTPUT", "response text\n⏺ Cost: $0.01 | Duration: 1s")

    client = SubprocessCopilotClient()
    result = asyncio.run(client.run(_TestGremlin("prompt", label="test")))

    assert result.text_result == "response text"


def test_strip_footer_preserves_bullet_in_body() -> None:
    text = "Result:\n⏺ Read(file.py)\n\nSome output\n⏺ Cost: $0.01 | Duration: 1s"
    assert _strip_footer(text) == "Result:\n⏺ Read(file.py)\n\nSome output"


def test_copilot_total_cost_usd_is_zero() -> None:
    client = SubprocessCopilotClient()
    assert client.total_cost_usd == 0.0


def test_copilot_run_accepts_extra_env_without_type_error(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")

    client = SubprocessCopilotClient()
    # Must not raise TypeError
    result = asyncio.run(
        client.run(_TestGremlin("prompt", label="x", extra_env={"FOO": "bar"}))
    )
    assert result.exit_code == 0


def test_copilot_extra_env_merged_into_subprocess_env(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    env_out = tmp_path / "env.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ENV_OUT", str(env_out))

    client = SubprocessCopilotClient()
    asyncio.run(client.run(_TestGremlin("prompt", label="x", extra_env={"FOO": "bar"})))

    captured = json.loads(env_out.read_text(encoding="utf-8"))
    assert captured.get("FOO") == "bar"


# ---------------------------------------------------------------------------
# native_block pass-through (rollout 8/9 of #582)
# ---------------------------------------------------------------------------
# Copilot's CLI has no per-tool flags, so the native block cannot be expressed
# as argv. The three tests below document the expected invocation shape for
# all three cases: non-empty block, bypass, and empty block.


def test_copilot_native_block_produces_base_argv(tmp_path, monkeypatch):
    """Non-empty native_block: no extra flags (copilot has no per-tool surface)."""
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    native_block = {"allowed_tools": ["Read", "Edit", "Bash", "Write", "Grep", "Glob"]}
    client = SubprocessCopilotClient(bypass=False, native_block=native_block)
    asyncio.run(client.run(_TestGremlin("the-prompt", label="test")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert argv == ["-p", "the-prompt"]


def test_copilot_bypass_still_passes_allow_all_with_native_block(tmp_path, monkeypatch):
    """bypass=True + native_block: --allow-all present (regression guard for #786)."""
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    native_block = {"allowed_tools": ["Read", "Edit"]}
    client = SubprocessCopilotClient(bypass=True, native_block=native_block)
    asyncio.run(client.run(_TestGremlin("the-prompt", label="test")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--allow-all" in argv


def test_copilot_empty_block_runs_with_no_extra_flags(tmp_path, monkeypatch):
    """Empty native_block: clean invocation, no --allow-all."""
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient(bypass=False, native_block={})
    asyncio.run(client.run(_TestGremlin("the-prompt", label="test")))

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert argv == ["-p", "the-prompt"]


def test_copilot_resume_replays_original_call(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    # Stub variant that appends each call's argv to a JSONL counter file so
    # we can assert resume() actually re-spawns the subprocess rather than
    # returning a cached CompletedRun.
    counter_stub = """\
import os, sys, json
calls_path = os.environ["STUB_CALLS_OUT"]
with open(calls_path, "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\\n")
sys.stdout.write(os.environ.get("STUB_OUTPUT", "hello from copilot"))
sys.stdout.flush()
"""
    _install_stub(bin_dir, counter_stub)
    calls_path = tmp_path / "calls.jsonl"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_OUTPUT", "copilot response")
    monkeypatch.setenv("STUB_CALLS_OUT", str(calls_path))

    # run() and resume() must share the same asyncio task so the per-task
    # ContextVar holding the resume payload survives between them.
    async def _drive() -> object:
        client = SubprocessCopilotClient()
        await client.run(_TestGremlin("first-prompt", label="test"))
        return await client.resume()

    result = asyncio.run(_drive())

    assert result.exit_code == 0
    assert result.text_result == "copilot response"
    calls = [json.loads(line) for line in calls_path.read_text().splitlines() if line]
    assert len(calls) == 2, f"expected resume() to spawn a second copilot, got {calls}"
    assert calls[0] == calls[1], "resume() must replay the original argv"
