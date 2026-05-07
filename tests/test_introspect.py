"""Tests for gremlins.stages.introspect."""

from __future__ import annotations

import pathlib
from collections.abc import Callable
from typing import Optional

import pytest

from gremlins.stages.introspect import build_launch_parser, stage_argspecs


# ---------------------------------------------------------------------------
# Minimal stub replacing the real StageEntry / Stage dependencies
# ---------------------------------------------------------------------------


class _FakeEntry:
    pass


# ---------------------------------------------------------------------------
# Fixture stage classes
# ---------------------------------------------------------------------------


class _AllTypes:
    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        a_str: str,
        a_int: int,
        a_float: float,
        a_bool: bool,
        a_path: pathlib.Path,
        opt_str: str | None = None,
        opt_path: Optional[pathlib.Path] = None,
    ) -> None:
        pass


class _RequiredAndOptional:
    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        required_param: str,
        optional_param: int = 42,
    ) -> None:
        pass


class _FrameworkOnly:
    """Has only framework params — should produce an empty list."""

    def __init__(self, entry: _FakeEntry, model: str | None) -> None:
        pass


class _WithCallable:
    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        hook: Callable[[str], None],
        name: str = "default",
    ) -> None:
        pass


class _UnsupportedAnnotation:
    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        bad: list[str],
    ) -> None:
        pass


class _WithOptionalBool:
    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        verbose: bool = False,
    ) -> None:
        pass


class _UnannotatedParam:
    def __init__(self, entry: _FakeEntry, model: str | None, *, name) -> None:
        pass


class _ConflictsWithInfra:
    """Stage param 'plan' collides with the infra --plan flag."""

    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        plan: str,
    ) -> None:
        pass


class _KebabStage:
    def __init__(
        self,
        entry: _FakeEntry,
        model: str | None,
        *,
        plan_file: pathlib.Path,
        base_url: str = "",
    ) -> None:
        pass


# ---------------------------------------------------------------------------
# stage_argspecs tests
# ---------------------------------------------------------------------------


def test_all_supported_types() -> None:
    specs = {s.name: s for s in stage_argspecs(_AllTypes)}
    assert specs["a_str"].type is str
    assert specs["a_int"].type is int
    assert specs["a_float"].type is float
    assert specs["a_bool"].type is bool
    assert specs["a_path"].type is pathlib.Path


def test_optional_unwrapped() -> None:
    specs = {s.name: s for s in stage_argspecs(_AllTypes)}
    assert specs["opt_str"].type is str
    assert specs["opt_path"].type is pathlib.Path


def test_required_true_when_no_default() -> None:
    specs = {s.name: s for s in stage_argspecs(_RequiredAndOptional)}
    assert specs["required_param"].required is True
    assert specs["required_param"].default is None


def test_required_false_when_default_present() -> None:
    specs = {s.name: s for s in stage_argspecs(_RequiredAndOptional)}
    assert specs["optional_param"].required is False
    assert specs["optional_param"].default == 42


def test_optional_default_is_none() -> None:
    specs = {s.name: s for s in stage_argspecs(_AllTypes)}
    assert specs["opt_str"].required is False
    assert specs["opt_str"].default is None


def test_framework_params_skipped() -> None:
    specs = stage_argspecs(_FrameworkOnly)
    assert specs == []


def test_self_entry_model_skipped() -> None:
    names = [s.name for s in stage_argspecs(_AllTypes)]
    assert "self" not in names
    assert "entry" not in names
    assert "model" not in names


def test_callable_param_skipped() -> None:
    names = [s.name for s in stage_argspecs(_WithCallable)]
    assert "hook" not in names
    assert "name" in names


def test_unsupported_annotation_raises() -> None:
    with pytest.raises(TypeError, match="bad"):
        stage_argspecs(_UnsupportedAnnotation)


def test_unannotated_param_raises() -> None:
    with pytest.raises(TypeError, match="name"):
        stage_argspecs(_UnannotatedParam)


def test_help_contains_type_str() -> None:
    specs = {s.name: s for s in stage_argspecs(_AllTypes)}
    assert "str" in specs["a_str"].help
    assert "int" in specs["a_int"].help
    assert "float" in specs["a_float"].help
    assert "bool" in specs["a_bool"].help
    assert "Path" in specs["a_path"].help


def test_help_optional_contains_none() -> None:
    specs = {s.name: s for s in stage_argspecs(_AllTypes)}
    assert "None" in specs["opt_str"].help


# ---------------------------------------------------------------------------
# build_launch_parser tests
# ---------------------------------------------------------------------------


def test_infra_flags_present() -> None:
    p = build_launch_parser("mypipe", _FrameworkOnly)
    actions = {a.dest for a in p._actions}
    assert "description" in actions
    assert "parent_id" in actions
    assert "print_id" in actions
    assert "base_ref" in actions
    assert "client" in actions
    assert "spec_path" in actions
    assert "plan" in actions


def test_prog_includes_pipeline_name() -> None:
    p = build_launch_parser("local", _FrameworkOnly)
    assert p.prog == "gremlins launch local"


def test_stage_flags_in_kebab_case() -> None:
    p = build_launch_parser("mypipe", _KebabStage)
    option_strings = [s for a in p._actions for s in a.option_strings]
    assert "--plan-file" in option_strings
    assert "--base-url" in option_strings
    assert "--plan_file" not in option_strings


def test_required_stage_flag_marked_required() -> None:
    p = build_launch_parser("mypipe", _KebabStage)
    required_flags = [
        a.option_strings[0]
        for a in p._actions
        if a.required
    ]
    assert "--plan-file" in required_flags


def test_optional_stage_flag_not_required() -> None:
    p = build_launch_parser("mypipe", _KebabStage)
    optional_actions = {
        a.option_strings[0]: a
        for a in p._actions
        if a.option_strings and not a.required
    }
    assert "--base-url" in optional_actions


def test_help_mentions_param_name_and_type(capsys: pytest.CaptureFixture[str]) -> None:
    p = build_launch_parser("mypipe", _AllTypes)
    with pytest.raises(SystemExit):
        p.parse_args(["--help"])
    out = capsys.readouterr().out
    for name in ("a-str", "a-int", "a-float", "a-bool", "a-path", "opt-str", "opt-path"):
        assert name in out, f"--{name} missing from --help output"
    for type_str in ("str", "int", "float", "bool", "Path"):
        assert type_str in out, f"type hint {type_str!r} missing from --help output"


def test_conflicting_name_raises() -> None:
    with pytest.raises(ValueError, match="plan"):
        build_launch_parser("mypipe", _ConflictsWithInfra)


def test_required_bool_parsed_from_string() -> None:
    p = build_launch_parser("mypipe", _AllTypes)
    args = p.parse_args(["--a-str", "x", "--a-int", "1", "--a-float", "1.0",
                         "--a-bool", "true", "--a-path", "/tmp"])
    assert args.a_bool is True
    args2 = p.parse_args(["--a-str", "x", "--a-int", "1", "--a-float", "1.0",
                          "--a-bool", "false", "--a-path", "/tmp"])
    assert args2.a_bool is False


def test_optional_bool_uses_boolean_optional_action() -> None:
    p = build_launch_parser("mypipe", _WithOptionalBool)
    assert p.parse_args([]).verbose is False
    assert p.parse_args(["--verbose"]).verbose is True
    assert p.parse_args(["--no-verbose"]).verbose is False
