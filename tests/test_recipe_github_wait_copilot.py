"""Tests for the github-wait-copilot YAML recipe."""

from __future__ import annotations

import pathlib
import textwrap
from typing import Any

from gremlins.pipeline.preprocess import expand_pipeline


def _make_pipeline(tmp_path: pathlib.Path, entry: str) -> dict[str, Any]:
    p = tmp_path / "pipeline.yaml"
    p.write_text(
        textwrap.dedent(f"""\
            default_client: claude:sonnet
            stages:
              {entry}
        """),
        encoding="utf-8",
    )
    return expand_pipeline(p)


def test_recipe_expands_to_loop(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: gremlins:github-wait-copilot, in: {pr: pr} }",
    )
    stages = result["stages"]
    assert len(stages) == 1
    loop = stages[0]
    assert loop["type"] == "loop"
    assert loop["name"] == "github-wait-copilot"


def test_recipe_body_has_one_exec_stage(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: gremlins:github-wait-copilot, in: {pr: pr} }",
    )
    body = result["stages"][0]["body"]
    assert len(body) == 1
    assert body[0]["name"] == "poll"
    assert body[0]["type"] == "exec"


def test_recipe_in_pr_number_on_body(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: gremlins:github-wait-copilot, in: {pr: pr} }",
    )
    body_in = result["stages"][0]["body"][0].get("in", {})
    assert body_in.get("pr_number") == "pr.number"


def test_recipe_default_max_iterations(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: gremlins:github-wait-copilot, in: {pr: pr} }",
    )
    loop = result["stages"][0]
    assert loop.get("max-iterations") == 40


def test_recipe_status_declared_on_body(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: gremlins:github-wait-copilot, in: {pr: pr} }",
    )
    body_out = result["stages"][0]["body"][0].get("out", {})
    assert "status" in body_out


def test_recipe_auto_resolved_by_type_name(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "- { type: github-wait-copilot, in: {pr: pr} }",
    )
    stages = result["stages"]
    assert len(stages) == 1
    assert stages[0]["type"] == "loop"
    assert stages[0]["name"] == "github-wait-copilot"
