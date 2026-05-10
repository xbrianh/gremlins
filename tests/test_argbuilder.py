"""Tests for gremlins.cli.build_launch_parser."""

from __future__ import annotations

import pytest

from gremlins.cli import build_launch_parser
from gremlins.stages import Stage, StageInput


def _stage_with(*inputs: StageInput) -> type[Stage]:
    class _Stage(Stage):
        def __init__(self) -> None:  # type: ignore[override]
            pass

        @classmethod
        def orchestration_args(cls) -> list[StageInput]:
            return list(inputs)

    return _Stage


_empty_stage = _stage_with()


def test_infra_flags_always_present() -> None:
    p = build_launch_parser("mypipe", _empty_stage)
    dests = {a.dest for a in p._actions}
    assert "description" in dests
    assert "parent_id" in dests
    assert "print_id" in dests
    assert "base_ref" in dests
    assert "client" in dests


def test_empty_orchestration_args_produces_no_extra_flags() -> None:
    p = build_launch_parser("mypipe", _empty_stage)
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--plan" not in flags
    assert "--instructions" not in flags


def test_prog_includes_pipeline_name() -> None:
    p = build_launch_parser("local", _empty_stage)
    assert p.prog == "gremlins launch local"


def test_stage_input_exposes_flag() -> None:
    stage = _stage_with(
        StageInput("plan", str, required=False, default=None, help="plan path")
    )
    p = build_launch_parser("mypipe", stage)
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--plan" in flags


def test_flag_name_kebab_cased() -> None:
    stage = _stage_with(
        StageInput("plan_file", str, required=False, default=None, help="")
    )
    p = build_launch_parser("mypipe", stage)
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--plan-file" in flags
    assert "--plan_file" not in flags


def test_required_flag_marked_required() -> None:
    stage = _stage_with(StageInput("topic", str, required=True, default=None, help=""))
    p = build_launch_parser("mypipe", stage)
    required_flags = [a.option_strings[0] for a in p._actions if a.required]
    assert "--topic" in required_flags


def test_optional_flag_not_required() -> None:
    stage = _stage_with(StageInput("topic", str, required=False, default="x", help=""))
    p = build_launch_parser("mypipe", stage)
    optional = {
        a.option_strings[0]: a
        for a in p._actions
        if a.option_strings and not a.required
    }
    assert "--topic" in optional


def test_bool_optional_uses_boolean_optional_action() -> None:
    stage = _stage_with(
        StageInput("verbose", bool, required=False, default=False, help="")
    )
    p = build_launch_parser("mypipe", stage)
    assert p.parse_args([]).verbose is False
    assert p.parse_args(["--verbose"]).verbose is True
    assert p.parse_args(["--no-verbose"]).verbose is False


def test_bool_required_parsed_from_string() -> None:
    stage = _stage_with(StageInput("flag", bool, required=True, default=None, help=""))
    p = build_launch_parser("mypipe", stage)
    assert p.parse_args(["--flag", "true"]).flag is True
    assert p.parse_args(["--flag", "false"]).flag is False


def test_infra_flag_collision_raises() -> None:
    stage = _stage_with(
        StageInput("description", str, required=False, default=None, help="")
    )
    with pytest.raises(ValueError, match="description"):
        build_launch_parser("mypipe", stage)


def test_client_collision_raises() -> None:
    stage = _stage_with(
        StageInput("client", str, required=False, default=None, help="")
    )
    with pytest.raises(ValueError, match="client"):
        build_launch_parser("mypipe", stage)


def test_plan_stage_inputs_expose_expected_flags() -> None:
    from gremlins.stages.plan import Plan

    p = build_launch_parser("local", Plan)
    flags = [s for a in p._actions for s in a.option_strings]
    assert "--instructions" in flags
    assert "--plan" in flags
    assert "--repo" in flags
