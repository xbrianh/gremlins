"""Tests for active_children rendering in fleet row and JSON output."""

from __future__ import annotations

from typing import Any

from gremlins.fleet.render import build_row
from gremlins.fleet.views import _gremlin_to_json  # type: ignore[reportPrivateUsage]


def _row(state: dict[str, Any]) -> Any:
    return build_row(
        "gr-abc123", "/fake/state.json", "/tmp/fake-wdir", state, "running"
    )


def test_build_row_single_active_child() -> None:
    row = _row({"stage": "parallel", "active_children": ["review-pr"]})
    assert row.stage == "review-pr"


def test_build_row_multiple_active_children() -> None:
    row = _row({"stage": "parallel", "active_children": ["a", "b"]})
    assert row.stage == "a+1"


def test_build_row_five_active_children() -> None:
    row = _row({"stage": "loop", "active_children": ["x"] * 5})
    assert row.stage == "x+4"


def test_build_row_long_leader_multiple_children() -> None:
    long_name = "x" * 30
    row = _row({"stage": "parallel", "active_children": [long_name, "y"]})
    assert row.stage == "x" * 20 + "+1"
    assert len(row.stage) == 22


def test_build_row_no_active_children_when_waiting() -> None:
    row = _row({"stage": "waiting", "active_children": ["some-child"]})
    assert "some-child" not in row.stage
    assert row.stage.startswith("waiting")


def test_build_row_no_active_children_field() -> None:
    row = _row({"stage": "parallel"})
    assert row.stage == "parallel"


def test_gremlin_to_json_includes_active_children() -> None:
    state: dict[str, Any] = {
        "stage": "parallel",
        "active_children": ["review-pr"],
        "started_at": "2026-01-01T00:00:00Z",
    }
    result = _gremlin_to_json("gr-abc123", "/tmp/fake-wdir", state, "running")
    assert result["active_children"] == ["review-pr"]


def test_gremlin_to_json_active_children_empty_when_absent() -> None:
    state: dict[str, Any] = {"stage": "parallel"}
    result = _gremlin_to_json("gr-abc123", "/tmp/fake-wdir", state, "running")
    assert result["active_children"] == []
