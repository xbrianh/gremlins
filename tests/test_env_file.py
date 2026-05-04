"""Tests for gremlins.env_file."""

from __future__ import annotations

import os

import pytest

from gremlins.env_file import load_env_file


def test_load_sets_new_var(tmp_path):
    env_file = tmp_path / "env"
    env_file.write_text("export GREMLIN_TEST_VAR=hello_world\n")
    result = load_env_file(env_file)
    assert result["GREMLIN_TEST_VAR"] == "hello_world"


def test_load_command_substitution(tmp_path):
    env_file = tmp_path / "env"
    env_file.write_text("export GREMLIN_SUBST=$(echo computed_value)\n")
    result = load_env_file(env_file)
    assert result["GREMLIN_SUBST"] == "computed_value"


def test_load_does_not_include_unchanged_vars(tmp_path):
    env_file = tmp_path / "env"
    existing_key = next(iter(os.environ))
    existing_val = os.environ[existing_key]
    env_file.write_text(f"export {existing_key}={existing_val}\n")
    result = load_env_file(env_file)
    assert existing_key not in result


def test_load_failure_raises(tmp_path):
    env_file = tmp_path / "env"
    env_file.write_text("exit 1\n")
    with pytest.raises(RuntimeError, match="failed to source"):
        load_env_file(env_file)


def test_load_syntax_error_raises(tmp_path):
    env_file = tmp_path / "env"
    env_file.write_text("(((\n")
    with pytest.raises(RuntimeError):
        load_env_file(env_file)
