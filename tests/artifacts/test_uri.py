"""Tests for gremlins.artifacts.uri."""
from __future__ import annotations

import pytest

from gremlins.artifacts.uri import Uri


@pytest.mark.parametrize(
    "s",
    [
        "file://session/foo.md",
        "git://range/abc..def",
        "git://ref/main",
        "git://commit/abc123",
        "gh://pr/42",
        "gh://issue/7",
    ],
)
def test_parse_roundtrip(s: str) -> None:
    uri = Uri.parse(s)
    assert str(uri) == s


def test_parse_unknown_scheme_raises() -> None:
    with pytest.raises(ValueError) as exc_info:
        Uri.parse("unknown://foo")
    msg = str(exc_info.value)
    assert "unknown" in msg
    assert "registered schemes" in msg


def test_parse_missing_separator_raises() -> None:
    with pytest.raises(ValueError):
        Uri.parse("no-slashes")


def test_str_roundtrip() -> None:
    uri = Uri(scheme="file", path="session/plan.md")
    assert str(uri) == "file://session/plan.md"
