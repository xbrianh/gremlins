"""Tests for State.format()."""

from __future__ import annotations

import dataclasses
import pathlib

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.stages.base import Stage


def _make_state(
    *,
    loop_iteration: int = 1,
    attempt: str = "",
    scope_names: list[str] | None = None,
    repo: str = "",
    cwd: str = "",
    base_ref: str = "",
):
    data = dataclasses.replace(
        StateData.load(None),
        loop_iteration=loop_iteration,
        attempt=attempt,
        base_ref_name=base_ref,
    )
    state = build_state(
        data=data,
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        repo=repo,
        cwd=cwd,
    )
    if scope_names:
        stages = [Stage(name) for name in scope_names]
        state = dataclasses.replace(state, current_scope=stages)
    return state


def test_format_n():
    s = _make_state(
        loop_iteration=3, attempt="abc", scope_names=["implement", "verify"]
    )
    assert s.format("handoff-{n}.md") == "handoff-3.md"


def test_format_attempt():
    s = _make_state(
        loop_iteration=3, attempt="abc", scope_names=["implement", "verify"]
    )
    assert s.format("{attempt}") == "abc"


def test_format_scope():
    s = _make_state(
        loop_iteration=3, attempt="abc", scope_names=["implement", "verify"]
    )
    assert s.format("{scope}") == "implement/verify"


def test_format_combined():
    s = _make_state(
        loop_iteration=3, attempt="abc", scope_names=["implement", "verify"]
    )
    assert s.format("{n}-{attempt}-{scope}") == "3-abc-implement/verify"


def test_format_empty_scope():
    s = _make_state(loop_iteration=1, attempt="x")
    assert s.format("{scope}") == ""
