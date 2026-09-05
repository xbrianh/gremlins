"""Tests for the verify YAML recipe."""

from __future__ import annotations

import pathlib
import textwrap
from typing import Any

import pytest
from _gremlins_core.discovery import resolve_pipeline_name as _resolve_pipeline_name
from _gremlins_core.schemas import expand_pipeline as _expand_pipeline

from gremlins.pipeline import Pipeline
from gremlins.pipelines import BUNDLED_PIPELINE_DIR
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.recipes import BUNDLED_STAGE_DEF_DIR


def _resolve(n, pr):
    return _resolve_pipeline_name(n, pr, BUNDLED_PIPELINE_DIR)


def _make_pipeline(tmp_path: pathlib.Path, verify_entry: str) -> dict[str, Any]:
    p = tmp_path / "pipeline.yaml"
    p.write_text(
        textwrap.dedent(f"""\
            default_client: openai:gpt-4o
            prompts:
              verify: gremlins:verify_fix.md
            stages:
              {verify_entry}
        """),
        encoding="utf-8",
    )
    return _expand_pipeline(
        str(p),
        None,
        str(BUNDLED_STAGE_DEF_DIR),
        str(BUNDLED_PROMPT_DIR),
        _resolve,
    )


def test_verify_recipe_expands_to_loop(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    stages = result["stages"]
    assert len(stages) == 1
    loop = stages[0]
    assert loop["type"] == "loop"
    assert loop.get("_auto_name") == "verify"
    assert "name" not in loop


def test_verify_recipe_body_has_two_stages(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    body = result["stages"][0]["body"]
    assert len(body) == 2
    assert body[0]["name"] == "cmd"
    assert body[0]["type"] == "exec"
    assert body[1]["name"] == "fix"
    assert body[1]["type"] == "agent"


def test_verify_cmds_wrapped_with_done_writing(tmp_path: pathlib.Path) -> None:
    """The cmd exec wraps cmds to always exit 0 and write done on success."""
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    cmd_stage = result["stages"][0]["body"][0]
    wrapped = cmd_stage["options"]["cmds"][0]
    assert "make check" in wrapped
    assert "printf 'done'" in wrapped
    assert "{artifact_dir}/done" in wrapped
    assert "true" in wrapped


def test_verify_cmds_wrapped_multi(tmp_path: pathlib.Path) -> None:
    """Multiple cmds are joined with && inside the wrapper."""
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check', 'make test'] }, prompt: verify }",
    )
    cmd_stage = result["stages"][0]["body"][0]
    wrapped = cmd_stage["options"]["cmds"][0]
    assert "make check && make test" in wrapped
    assert "printf 'done'" in wrapped


def test_verify_empty_cmds_raises(tmp_path: pathlib.Path) -> None:
    with pytest.raises(ValueError, match="cmds"):
        _make_pipeline(
            tmp_path,
            "- { type: verify, options: { cmds: [] } }",
        )


def test_verify_missing_cmds_raises(tmp_path: pathlib.Path) -> None:
    with pytest.raises(ValueError, match="cmds"):
        _make_pipeline(
            tmp_path,
            "- { type: verify }",
        )


def test_verify_fix_has_skip_if_exists(tmp_path: pathlib.Path) -> None:
    """The fix agent has skip_if_exists: done so it's skipped when check passed."""
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    fix_stage = result["stages"][0]["body"][1]
    assert fix_stage.get("skip_if_exists") == "done"
    assert isinstance(fix_stage.get("prompt"), list)
    assert len(fix_stage["prompt"]) >= 1


def test_repeated_verify_recipe_deduplicates_names(tmp_path: pathlib.Path) -> None:
    """Two recipe invocations without explicit name: produce verify and verify-2."""
    p = tmp_path / "pipeline.yaml"
    p.write_text(
        textwrap.dedent("""\
            default_client: openai:gpt-4o
            prompts:
              verify: gremlins:verify_fix.md
            stages:
              - { type: verify, options: { cmds: ["true"] }, prompt: verify }
              - { type: verify, options: { cmds: ["true"] }, prompt: verify }
        """),
        encoding="utf-8",
    )
    pipeline = Pipeline.from_yaml(p)
    assert len(pipeline.stages) == 2
    assert pipeline.stages[0].name == "verify"
    assert pipeline.stages[1].name == "verify-2"
