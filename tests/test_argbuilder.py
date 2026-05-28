"""Tests for gremlins.cli.build_launch_parser."""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from gremlins.cli.launch import build_launch_parser
from gremlins.pipeline import Pipeline
from gremlins.stages.exec import Exec


def _pipeline_with_inputs(in_map: dict[str, str] | None) -> Pipeline:
    inputs_stage = None
    if in_map is not None:
        inputs_stage = Exec("inputs", {}, in_map=in_map)
    p = MagicMock(spec=Pipeline)
    p.inputs = inputs_stage
    return p


_empty_pipeline = _pipeline_with_inputs(None)


def test_infra_flags_always_present() -> None:
    p = build_launch_parser("mypipe", _empty_pipeline)
    dests = {a.dest for a in p._actions}
    assert "description" in dests
    assert "parent_id" in dests
    assert "print_id" in dests
    assert "base_ref" in dests
    assert "client" in dests


def test_empty_inputs_produces_no_extra_flags() -> None:
    p = build_launch_parser("mypipe", _empty_pipeline)
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--plan" not in flags
    assert "--instructions" not in flags


def test_prog_includes_pipeline_name() -> None:
    p = build_launch_parser("local", _empty_pipeline)
    assert p.prog == "gremlins launch local"


def test_pipeline_input_exposes_flag() -> None:
    p = build_launch_parser("mypipe", _pipeline_with_inputs({"PLAN": "plan?"}))
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--plan" in flags


def test_flag_name_kebab_cased() -> None:
    p = build_launch_parser(
        "mypipe", _pipeline_with_inputs({"PLAN_FILE": "plan_file?"})
    )
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--plan-file" in flags
    assert "--plan_file" not in flags


def test_required_flag_marked_required() -> None:
    p = build_launch_parser("mypipe", _pipeline_with_inputs({"TOPIC": "topic"}))
    required_flags = [a.option_strings[0] for a in p._actions if a.required]
    assert "--topic" in required_flags


def test_optional_flag_not_required() -> None:
    p = build_launch_parser(
        "mypipe", _pipeline_with_inputs({"TOPIC": "topic?default_val"})
    )
    optional = {
        a.option_strings[0]: a
        for a in p._actions
        if a.option_strings and not a.required
    }
    assert "--topic" in optional


def test_optional_flag_default_value() -> None:
    p = build_launch_parser(
        "mypipe", _pipeline_with_inputs({"TOPIC": "topic?mydefault"})
    )
    assert p.parse_args([]).topic == "mydefault"


def test_optional_flag_no_default_is_none() -> None:
    p = build_launch_parser("mypipe", _pipeline_with_inputs({"TOPIC": "topic?"}))
    assert p.parse_args([]).topic is None


def test_infra_flag_collision_raises() -> None:
    with pytest.raises(ValueError, match="description"):
        build_launch_parser(
            "mypipe", _pipeline_with_inputs({"DESCRIPTION": "description?"})
        )


def test_client_collision_raises() -> None:
    with pytest.raises(ValueError, match="client"):
        build_launch_parser("mypipe", _pipeline_with_inputs({"CLIENT": "client?"}))


def test_none_pipeline_produces_only_infra_flags() -> None:
    p = build_launch_parser("mypipe", None)
    dests = {a.dest for a in p._actions}
    assert "description" in dests
    assert "base_ref" in dests


def test_pipeline_with_dotted_path_uses_registry_key() -> None:
    # pr.url -> registry key is "pr", flag is --pr
    p = build_launch_parser("mypipe", _pipeline_with_inputs({"PR_URL": "pr?"}))
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--pr" in flags
