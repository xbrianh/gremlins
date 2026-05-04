"""Tests for gremlins.pipeline loader."""

from __future__ import annotations

import pathlib

import pytest

import gremlins.pipeline  # noqa: F401
from gremlins.pipeline import load_pipeline


def _write_yaml(path: pathlib.Path, content: str) -> pathlib.Path:
    path.write_text(content, encoding="utf-8")
    return path


def _make_prompt(tmp_path: pathlib.Path, name: str = "prompt.md") -> pathlib.Path:
    p = tmp_path / name
    p.write_text("prompt content", encoding="utf-8")
    return p


# ---- valid pipeline --------------------------------------------------------


def test_valid_pipeline_parses(tmp_path: pathlib.Path) -> None:
    prompt = _make_prompt(tmp_path)
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        f"""\
name: test-pipe
clients:
  c1: {{provider: claude, model: sonnet}}
stages:
  - name: plan
    type: plan
    client: c1
    prompt: {prompt.name}
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert pipeline.name == "test-pipe"
    assert len(pipeline.stages) == 1
    assert pipeline.stages[0].name == "plan"
    assert pipeline.stages[0].type == "plan"
    assert pipeline.stages[0].client_key == "c1"
    assert pipeline.stages[0].prompt_paths == [prompt]
    assert "c1" in pipeline.clients
    assert pipeline.clients["c1"] is not None


# ---- error cases -----------------------------------------------------------


def test_unknown_stage_type_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
clients: {}
stages:
  - {name: s1, type: no-such-type}
""",
    )
    with pytest.raises(ValueError, match="unknown type"):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_unknown_client_key_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
clients:
  c1: {provider: claude, model: sonnet}
stages:
  - {name: s1, type: plan, client: missing_key}
""",
    )
    with pytest.raises(ValueError, match="unknown client key"):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_missing_prompt_file_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
clients: {}
stages:
  - {name: s1, type: verify, prompt: does_not_exist.md}
""",
    )
    with pytest.raises(FileNotFoundError):
        load_pipeline(tmp_path / "pipeline.yaml")


# ---- prompt list -----------------------------------------------------------


def test_prompt_list_resolves_both_paths(tmp_path: pathlib.Path) -> None:
    a = _make_prompt(tmp_path, "a.md")
    b = _make_prompt(tmp_path, "b.md")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        f"""\
name: p
clients: {{}}
stages:
  - name: s1
    type: verify
    prompt: [{a.name}, {b.name}]
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].prompt_paths == [a, b]


# ---- repeated type with distinct names ------------------------------------


def test_repeated_type_distinct_names(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - {name: test-pre, type: verify}
  - {name: test-post, type: verify}
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert len(pipeline.stages) == 2
    assert pipeline.stages[0].name == "test-pre"
    assert pipeline.stages[1].name == "test-post"
    assert pipeline.stages[0].type == "verify"
    assert pipeline.stages[1].type == "verify"


# ---- parallel group validation --------------------------------------------


def test_parallel_duplicate_child_name_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - name: reviews
    parallel:
      - {name: r1, type: verify}
      - {name: r1, type: verify}
""",
    )
    with pytest.raises(ValueError, match="duplicate child name"):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_nested_parallel_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - name: outer
    parallel:
      - name: inner
        parallel:
          - {name: leaf, type: verify}
""",
    )
    with pytest.raises(ValueError, match="nested parallel"):
        load_pipeline(tmp_path / "pipeline.yaml")
