"""Tests for pipeline name/path resolution and stage-type registry coverage."""

from __future__ import annotations

import pathlib

import pytest
from _gremlins_core.discovery import resolve_pipeline_name, resolve_pipeline_path
from _gremlins_core.schemas import STAGE_TYPES
from _gremlins_core.schemas import Pipeline as _PipelineData

_SAMPLE_YAML = """\
default_client: openai:gpt-4o
stages:
  - name: plan
    type: agent
"""


def test_pipeline_name_from_stem(tmp_path: pathlib.Path) -> None:
    yaml_path = tmp_path / "mypipe.yaml"
    yaml_path.write_text(_SAMPLE_YAML, encoding="utf-8")
    pipeline = _PipelineData.from_yaml(yaml_path)
    assert pipeline.name == "mypipe"


def test_pipeline_name_ignores_yaml_name_field(tmp_path: pathlib.Path) -> None:
    yaml_path = tmp_path / "mypipe.yaml"
    yaml_path.write_text("name: something-else\n" + _SAMPLE_YAML, encoding="utf-8")
    pipeline = _PipelineData.from_yaml(yaml_path)
    assert pipeline.name == "mypipe"


def _make_overlay(tmp_path: pathlib.Path, name: str) -> pathlib.Path:
    overlay = tmp_path / "overlay"
    overlay.mkdir(parents=True)
    (overlay / f"{name}.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    return overlay


def test_resolve_pipeline_name_uses_overlay_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    overlay = _make_overlay(tmp_path, "mylocal")
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(overlay))
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(tmp_path / "project"))
    result = resolve_pipeline_name("mylocal")
    assert result == (overlay / "mylocal.yaml").resolve()


def test_resolve_pipeline_path_uses_overlay_dir(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    overlay = _make_overlay(tmp_path, "mylocal")
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(overlay))
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(tmp_path / "project"))
    result = resolve_pipeline_path("mylocal")
    assert result == (overlay / "mylocal.yaml").resolve()


def test_resolve_pipeline_name_finds_project_dir_when_overlay_empty(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    empty_overlay = tmp_path / "empty_overlay"
    empty_overlay.mkdir()
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(empty_overlay))
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "mylocal.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(project_root))
    result = resolve_pipeline_name("mylocal")
    assert result == (pipeline_dir / "mylocal.yaml").resolve()


def test_resolve_pipeline_path_finds_project_dir_when_overlay_empty(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    empty_overlay = tmp_path / "empty_overlay"
    empty_overlay.mkdir()
    monkeypatch.setenv("GREMLINS_OVERLAY_DIR", str(empty_overlay))
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "mylocal.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(project_root))
    result = resolve_pipeline_path("mylocal")
    assert result == (pipeline_dir / "mylocal.yaml").resolve()


def test_resolve_pipeline_name_no_overlay_env_falls_through(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "local.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(project_root))
    result = resolve_pipeline_name("local")
    assert result == (pipeline_dir / "local.yaml").resolve()


def test_resolve_pipeline_path_no_overlay_env_falls_through(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    project_root = tmp_path / "project"
    pipeline_dir = project_root / ".gremlins"
    pipeline_dir.mkdir(parents=True)
    (pipeline_dir / "local.yaml").write_text(_SAMPLE_YAML, encoding="utf-8")
    monkeypatch.setenv("GREMLINS_PROJECT_ROOT", str(project_root))
    result = resolve_pipeline_path("local")
    assert result == (pipeline_dir / "local.yaml").resolve()


def test_stage_builders_registry_covers_all_known_types() -> None:
    expected = {
        "loop",
        "parallel",
    }
    assert expected <= set(STAGE_TYPES)
