import pytest

from gremlins.pipeline.preprocess import (
    _resolve_placeholder,  # type: ignore[reportPrivateUsage]
)


def test_key_present_returns_value() -> None:
    assert (
        _resolve_placeholder("options.interval", {"options": {"interval": "30"}})
        == "30"
    )


def test_key_absent_with_string_default_returns_default() -> None:
    assert (
        _resolve_placeholder('options.interval | default("20")', {"options": {}})
        == "20"
    )


def test_key_absent_with_bare_default_returns_default() -> None:
    assert (
        _resolve_placeholder("options.max_iterations | default(40)", {"options": {}})
        == "40"
    )


def test_key_present_with_default_returns_value_not_default() -> None:
    ctx = {"options": {"max_iterations": "10"}}
    assert _resolve_placeholder("options.max_iterations | default(40)", ctx) == "10"


def test_key_present_with_int_returns_str() -> None:
    assert (
        _resolve_placeholder("options.interval | default(20)", {"options": {"interval": 5}})
        == "5"
    )


def test_key_absent_no_default_raises() -> None:
    with pytest.raises(ValueError, match="not found in context"):
        _resolve_placeholder("options.missing", {"options": {}})
