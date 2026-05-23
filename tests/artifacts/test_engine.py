"""Tests for gremlins.artifacts.engine."""
from __future__ import annotations

from gremlins.artifacts.engine import EngineContext


def test_format_n() -> None:
    ctx = EngineContext(loop_iteration=3, attempt="abc", current_scope=["implement", "verify"])
    assert ctx.format("handoff-{n}.md") == "handoff-3.md"


def test_format_attempt() -> None:
    ctx = EngineContext(loop_iteration=3, attempt="abc", current_scope=["implement", "verify"])
    assert ctx.format("{attempt}") == "abc"


def test_format_scope() -> None:
    ctx = EngineContext(loop_iteration=3, attempt="abc", current_scope=["implement", "verify"])
    assert ctx.format("{scope}") == "implement/verify"


def test_format_combined() -> None:
    ctx = EngineContext(loop_iteration=3, attempt="abc", current_scope=["implement", "verify"])
    assert ctx.format("{n}-{attempt}-{scope}") == "3-abc-implement/verify"


def test_format_empty_scope() -> None:
    ctx = EngineContext(loop_iteration=1, attempt="x", current_scope=[])
    assert ctx.format("{scope}") == ""
