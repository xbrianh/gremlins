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
default_client: claude:sonnet
prompt_dir: .
stages:
  - name: plan
    type: plan
    prompt: {prompt.name}
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert pipeline.name == "test-pipe"
    assert pipeline.default_client is not None
    assert str(pipeline.default_client) == "claude:sonnet"
    assert len(pipeline.stages) == 1
    assert pipeline.stages[0].name == "plan"
    assert pipeline.stages[0].type == "plan"
    assert pipeline.stages[0].client is None
    assert pipeline.stages[0].prompt_paths == [prompt]


# ---- per-stage client override ---------------------------------------------


def test_per_stage_client_override(tmp_path: pathlib.Path) -> None:
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
default_client: claude:sonnet
stages:
  - name: implement
    type: implement
  - name: review
    type: implement
    client: copilot:gpt-5.4
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert pipeline.stages[0].client is None
    assert pipeline.stages[1].client is not None
    assert str(pipeline.stages[1].client) == "copilot:gpt-5.4"


# ---- error cases -----------------------------------------------------------


def test_unknown_stage_type_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
stages:
  - {name: s1, type: no-such-type}
""",
    )
    with pytest.raises(ValueError, match="unknown type"):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_unknown_provider_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
default_client: unknown:model
stages:
  - {name: s1, type: implement}
""",
    )
    with pytest.raises(ValueError, match="unknown provider"):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_invalid_client_format_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
stages:
  - name: s1
    type: implement
    client: notaspecifier
""",
    )
    with pytest.raises(ValueError):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_missing_prompt_file_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: bad
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
prompt_dir: .
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


# ---- client field population -----------------------------------------------


def test_client_spec_inherits_from_default(tmp_path: pathlib.Path) -> None:
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
default_client: claude:sonnet
stages:
  - name: s1
    type: implement
""",
    )
    pipeline = load_pipeline(yaml_path)
    # client is None at load time; resolution happens at run time
    assert pipeline.stages[0].client is None


def test_client_spec_stage_override_wins(tmp_path: pathlib.Path) -> None:
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
default_client: claude:sonnet
stages:
  - name: s1
    type: implement
    client: copilot:gpt-5.4
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert str(pipeline.stages[0].client) == "copilot:gpt-5.4"


def test_client_spec_none_when_no_default_no_stage(tmp_path: pathlib.Path) -> None:
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
stages:
  - name: s1
    type: implement
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert pipeline.stages[0].client is None


def test_client_spec_parallel_group_is_none(tmp_path: pathlib.Path) -> None:
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
default_client: claude:sonnet
stages:
  - name: reviews
    parallel:
      - name: r1
        type: verify
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert pipeline.stages[0].client is None
    # child has no explicit client; resolution happens at run time
    assert pipeline.stages[0].children[0].client is None
