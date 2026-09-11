"""Tests for gremlins.env_file."""

from __future__ import annotations

import os

import pytest

from gremlins.env_file import load_env_file_isolated

# ---------------------------------------------------------------------------
# load_env_file_isolated
# ---------------------------------------------------------------------------


def test_isolated_captures_full_env(tmp_path):
    """load_env_file_isolated returns base_env vars plus sourced vars."""
    env_file = tmp_path / "env"
    env_file.write_text("export FOO=bar\n")
    base = {"PATH": os.environ.get("PATH", ""), "HOME": "/fake/home"}
    result = load_env_file_isolated(env_file, base_env=base)
    assert result["FOO"] == "bar"
    assert result["PATH"] == base["PATH"]
    assert result["HOME"] == "/fake/home"


def test_isolated_no_parent_leak(tmp_path, monkeypatch):
    """Parent env vars not in base_env do not appear in the result."""
    monkeypatch.setenv("LEAK_ME", "yes")
    env_file = tmp_path / "env"
    env_file.write_text("export FOO=bar\n")
    base = {"PATH": os.environ.get("PATH", ""), "HOME": os.environ.get("HOME", "")}
    result = load_env_file_isolated(env_file, base_env=base)
    assert "LEAK_ME" not in result
    assert result["FOO"] == "bar"


def test_isolated_base_env_propagated(tmp_path):
    """base_env vars appear in the result."""
    env_file = tmp_path / "env"
    env_file.write_text("# empty\n")
    base = {"PATH": os.environ.get("PATH", ""), "HOME": "/custom_home"}
    result = load_env_file_isolated(env_file, base_env=base)
    assert result["PATH"] == base["PATH"]
    assert result["HOME"] == "/custom_home"


def test_isolated_syntax_error_raises(tmp_path):
    """Bash syntax errors raise RuntimeError."""
    env_file = tmp_path / "env"
    env_file.write_text("(((((\n")
    base = {"PATH": os.environ.get("PATH", ""), "HOME": "/h"}
    with pytest.raises(RuntimeError):
        load_env_file_isolated(env_file, base_env=base)


def test_isolated_bash_internals_stripped(tmp_path):
    """Bash internal vars (_, BASH, SHLVL, etc.) are absent."""
    env_file = tmp_path / "env"
    env_file.write_text("export MY_VAR=hello\n")
    base = {"PATH": os.environ.get("PATH", ""), "HOME": "/h"}
    result = load_env_file_isolated(env_file, base_env=base)
    assert "_" not in result
    assert "BASH" not in result
    assert "SHLVL" not in result
    assert result["MY_VAR"] == "hello"


def test_isolated_cwd(tmp_path):
    """cwd parameter works — $(pwd) resolves to the specified directory."""
    subdir = tmp_path / "sub"
    subdir.mkdir()
    env_file = tmp_path / "env"
    env_file.write_text("export CWD=$(pwd)\n")
    base = {"PATH": os.environ.get("PATH", ""), "HOME": "/h"}
    result = load_env_file_isolated(env_file, base_env=base, cwd=subdir)
    assert result["CWD"] == str(subdir)
