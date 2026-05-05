"""Tests for SubprocessCopilotClient (gremlins/clients/copilot.py)."""

from __future__ import annotations

import json
import os
import pathlib
import sys

import pytest

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
    result = client.run("my prompt", label="test")

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
    client.run("the prompt", label="test")

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    p_idx = argv.index("-p")
    assert argv[p_idx + 1] == "the prompt"


def test_copilot_client_passes_allow_all_tools_flag(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient()
    client.run("prompt", label="test")

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--allow-all-tools" in argv
    assert "-p" in argv


def test_copilot_client_passes_model_when_specified(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    argv_out = tmp_path / "argv.json"

    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_ARGV_OUT", str(argv_out))

    client = SubprocessCopilotClient()
    client.run("prompt", label="test", model="gpt-5.4")

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
    client.run("prompt", label="test", model=None)

    argv = json.loads(argv_out.read_text(encoding="utf-8"))
    assert "--model" not in argv


def test_copilot_client_raises_on_nonzero_exit(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _FAIL_STUB_SRC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_EXIT_CODE", "2")

    client = SubprocessCopilotClient()
    with pytest.raises(RuntimeError, match="copilot -p.*exited 2"):
        client.run("prompt", label="test")


def test_copilot_client_strips_footer(tmp_path, monkeypatch):
    bin_dir = tmp_path / "bin"
    _install_stub(bin_dir, _STUB_COPILOT_SRC)
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}")
    monkeypatch.setenv("STUB_OUTPUT", "response text\n⏺ Cost: $0.01 | Duration: 1s")

    client = SubprocessCopilotClient()
    result = client.run("prompt", label="test")

    assert result.text_result == "response text"


def test_strip_footer_preserves_bullet_in_body() -> None:
    text = "Result:\n⏺ Read(file.py)\n\nSome output\n⏺ Cost: $0.01 | Duration: 1s"
    assert _strip_footer(text) == "Result:\n⏺ Read(file.py)\n\nSome output"


def test_copilot_total_cost_usd_is_zero() -> None:
    client = SubprocessCopilotClient()
    assert client.total_cost_usd == 0.0
