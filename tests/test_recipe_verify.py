"""Tests for the verify YAML recipe."""

from __future__ import annotations

import pathlib
import textwrap
from typing import Any

import pytest

from gremlins.pipeline.preprocess import expand_pipeline


def _make_pipeline(tmp_path: pathlib.Path, verify_entry: str) -> dict[str, Any]:
    p = tmp_path / "pipeline.yaml"
    p.write_text(
        textwrap.dedent(f"""\
            default_client: claude:sonnet
            prompts:
              verify: gremlins:verify_fix.md
            stages:
              {verify_entry}
        """),
        encoding="utf-8",
    )
    return expand_pipeline(p)


def test_verify_recipe_expands_to_loop(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    stages = result["stages"]
    assert len(stages) == 1
    loop = stages[0]
    assert loop["type"] == "loop"
    assert loop["name"] == "verify"


def test_verify_recipe_body_has_three_stages(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    body = result["stages"][0]["body"]
    assert len(body) == 3
    assert body[0]["name"] == "cmd"
    assert body[0]["type"] == "exec"
    assert body[1]["name"] == "verify-diff"
    assert body[1]["type"] == "exec"
    assert body[2]["name"] == "fix"
    assert body[2]["type"] == "agent"


def test_verify_cmds_propagated_to_cmd_stage(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check', 'make test'] }, prompt: verify }",
    )
    cmd_stage = result["stages"][0]["body"][0]
    assert cmd_stage["options"]["cmds"] == ["make check", "make test"]


def test_verify_diff_keeps_own_cmds(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    diff_stage = result["stages"][0]["body"][1]
    assert any("git diff HEAD" in c for c in diff_stage["options"]["cmds"])


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


def test_verify_prompt_reaches_fix_agent(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: verify, options: { cmds: ['make check'] }, prompt: verify }",
    )
    fix_stage = result["stages"][0]["body"][2]
    assert isinstance(fix_stage.get("prompt"), list)
    assert len(fix_stage["prompt"]) >= 1
    assert "verify" in fix_stage["prompt"][0].lower()
