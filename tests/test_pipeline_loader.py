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
    assert pipeline.stages[0].prompts == ["prompt content"]


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


# ---- gremlins: prefix for bundled prompts ---------------------------------


def test_bundled_prefix_resolves_to_package(tmp_path: pathlib.Path) -> None:
    """`gremlins:NAME` resolves from the bundled prompts dir."""
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
stages:
  - name: s1
    type: verify
    prompt: [gremlins:code_style.md]
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert len(pipeline.stages[0].prompts) == 1
    assert pipeline.stages[0].prompts[0].strip()  # non-empty content


def test_bare_name_does_not_fall_back_to_bundled(tmp_path: pathlib.Path) -> None:
    """Bare names resolve only from prompt_dir, never from bundled."""
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
stages:
  - name: s1
    type: verify
    prompt: [code_style.md]
""",
    )
    with pytest.raises(FileNotFoundError):
        load_pipeline(tmp_path / "pipeline.yaml")


def test_mixed_bundled_and_local_prompts(tmp_path: pathlib.Path) -> None:
    """Single stage may mix `gremlins:` and bare-name prompts."""
    local = _make_prompt(tmp_path, "local.md")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        f"""\
name: p
prompt_dir: .
stages:
  - name: s1
    type: verify
    prompt: [gremlins:code_style.md, {local.name}]
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert len(pipeline.stages[0].prompts) == 2
    assert pipeline.stages[0].prompts[1] == "prompt content"


def test_bundled_prefix_without_name_raises(tmp_path: pathlib.Path) -> None:
    """`gremlins:` with no name after the prefix is rejected at load time."""
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
stages:
  - name: s1
    type: verify
    prompt: ["gremlins:"]
""",
    )
    with pytest.raises(ValueError, match="missing a name"):
        load_pipeline(tmp_path / "pipeline.yaml")


# ---- named prompts (top-level prompts: mapping) ----------------------------


def test_named_prompt_single_file(tmp_path: pathlib.Path) -> None:
    _make_prompt(tmp_path, "base.md")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
prompts:
  base: base.md
stages:
  - name: s1
    type: verify
    prompt: base
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].prompts == ["prompt content"]


def test_named_prompt_list_of_files(tmp_path: pathlib.Path) -> None:
    _make_prompt(tmp_path, "a.md")
    (tmp_path / "b.md").write_text("other content", encoding="utf-8")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
prompts:
  combo: [a.md, b.md]
stages:
  - name: s1
    type: verify
    prompt: combo
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].prompts == ["prompt content", "other content"]


def test_named_prompt_mixed_with_bare_path(tmp_path: pathlib.Path) -> None:
    _make_prompt(tmp_path, "base.md")
    (tmp_path / "extra.md").write_text("extra content", encoding="utf-8")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
prompts:
  base: base.md
stages:
  - name: s1
    type: verify
    prompt: [base, extra.md]
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].prompts == ["prompt content", "extra content"]


def test_named_prompt_mixed_with_bundled(tmp_path: pathlib.Path) -> None:
    _make_prompt(tmp_path, "local.md")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
prompts:
  local-base: local.md
stages:
  - name: s1
    type: verify
    prompt: [local-base, gremlins:code_style.md]
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert len(pipeline.stages[0].prompts) == 2
    assert pipeline.stages[0].prompts[0] == "prompt content"
    assert pipeline.stages[0].prompts[1].strip()


def test_named_prompt_shared_across_stages(tmp_path: pathlib.Path) -> None:
    _make_prompt(tmp_path, "shared.md")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
prompts:
  shared: shared.md
stages:
  - name: s1
    type: verify
    prompt: shared
  - name: s2
    type: verify
    prompt: shared
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].prompts == ["prompt content"]
    assert pipeline.stages[1].prompts == ["prompt content"]


def test_named_prompt_bundled_value(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompts:
  style: gremlins:code_style.md
stages:
  - name: s1
    type: verify
    prompt: style
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert len(pipeline.stages[0].prompts) == 1
    assert pipeline.stages[0].prompts[0].strip()


# ---- prompt list -----------------------------------------------------------


def test_prompt_list_resolves_both_paths(tmp_path: pathlib.Path) -> None:
    _make_prompt(tmp_path, "a.md")
    _make_prompt(tmp_path, "b.md")
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
prompt_dir: .
stages:
  - name: s1
    type: verify
    prompt: [a.md, b.md]
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].prompts == ["prompt content", "prompt content"]


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
    assert pipeline.stages[0].body[0].client is None


# ---- include: directive ---------------------------------------------------


def test_include_directive_expands_bundled_pipeline(tmp_path: pathlib.Path) -> None:
    """{ include: local } inlines all stages from the bundled local pipeline."""
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
stages:
  - { include: local }
""",
    )
    pipeline = load_pipeline(yaml_path)
    stage_types = [s.type for s in pipeline.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert "verify" in stage_types


def test_include_unknown_pipeline_raises(tmp_path: pathlib.Path) -> None:
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
stages:
  - { include: no-such-pipeline }
""",
    )
    with pytest.raises(FileNotFoundError, match="no-such-pipeline"):
        load_pipeline(yaml_path)


def test_loop_body_with_include_expands(tmp_path: pathlib.Path) -> None:
    """Loop body supports { include: <name> } to inline another pipeline's stages."""
    yaml_path = _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
stages:
  - name: myloop
    type: loop
    body:
      - { name: handoff, type: handoff }
      - { include: local }
""",
    )
    pipeline = load_pipeline(yaml_path)
    assert len(pipeline.stages) == 1
    loop = pipeline.stages[0]
    assert loop.type == "loop"
    assert loop.body[0].type == "handoff"
    body_types = [b.type for b in loop.body]
    assert "plan" in body_types
    assert "implement" in body_types


def test_boss_yaml_loads() -> None:
    """boss.yaml loads with loop/handoff structure replacing the old chain stage."""
    from gremlins.pipeline import load_pipeline, resolve_pipeline_path

    pipeline = load_pipeline(resolve_pipeline_path("boss", pathlib.Path.cwd()))
    names = [s.name for s in pipeline.stages]
    assert names == ["chain", "review-chain", "address-chain"]
    chain_entry = pipeline.stages[0]
    assert chain_entry.type == "loop"
    body_types = [b.type for b in chain_entry.body]
    assert body_types[0] == "handoff"
    assert "implement" in body_types
