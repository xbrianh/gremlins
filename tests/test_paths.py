import pathlib

import pytest

from gremlins import paths


def test_work_root_sandbox(tmp_path, monkeypatch):
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    result = paths.work_root()
    assert result == tmp_path / "work"
    assert result.exists()


def test_work_root_default(monkeypatch):
    monkeypatch.delenv("GREMLINS_SANDBOX_ROOT", raising=False)
    result = paths.work_root()
    assert result.is_absolute()
    assert result.exists()


def test_user_config_root_sandbox(tmp_path, monkeypatch):
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))
    assert paths.user_config_root() == tmp_path / "config"


def test_user_config_root_default(monkeypatch):
    monkeypatch.delenv("GREMLINS_SANDBOX_ROOT", raising=False)
    result = paths.user_config_root()
    assert result == pathlib.Path.home() / ".config" / "gremlins"


def test_project_root_override(tmp_path, monkeypatch):
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(tmp_path))
    assert paths.project_root() == tmp_path


def test_project_root_default(monkeypatch, tmp_path):
    monkeypatch.delenv("GREMLINS_PROJECT_ROOT", raising=False)
    monkeypatch.chdir(tmp_path)
    assert paths.project_root() == tmp_path


def test_expand_user_path_tilde(monkeypatch, tmp_path):
    monkeypatch.setenv("HOME", str(tmp_path))
    result = paths.expand_user_path("~/foo")
    assert result == str(tmp_path / "foo")


def test_expand_user_path_no_tilde():
    assert paths.expand_user_path("/absolute/path") == "/absolute/path"
