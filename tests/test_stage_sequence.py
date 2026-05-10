"""Tests for SequenceStage."""

from __future__ import annotations

import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import RuntimeState
from gremlins.stages.sequence import SequenceStage


def _state() -> RuntimeState:
    return RuntimeState(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
    )


def _make_sequence(
    runners: list, parent_state: RuntimeState | None = None
) -> tuple[SequenceStage, list, RuntimeState]:
    body = [(_state(), fn) for fn in runners]
    stage = SequenceStage("seq", body=body)
    return stage, body, parent_state or _state()


def test_sequence_runs_body_in_order() -> None:
    log: list[str] = []
    stage, _, parent_state = _make_sequence(
        [lambda: log.append("a"), lambda: log.append("b"), lambda: log.append("c")]
    )
    stage.run(parent_state)
    assert log == ["a", "b", "c"]


def test_sequence_stops_on_exception() -> None:
    log: list[str] = []

    def fail() -> None:
        raise RuntimeError("boom")

    stage, _, parent_state = _make_sequence(
        [lambda: log.append("a"), fail, lambda: log.append("c")]
    )
    with pytest.raises(RuntimeError, match="boom"):
        stage.run(parent_state)
    assert log == ["a"]
    assert "c" not in log


def test_sequence_propagates_worktree() -> None:
    observed: list[pathlib.Path | None] = []
    wt = pathlib.Path("/tmp/fake-worktree")

    parent_state = _state()
    parent_state.worktree = wt

    sub_state = _state()

    def capture() -> None:
        observed.append(sub_state.worktree)

    stage = SequenceStage("seq", body=[(sub_state, capture)])
    stage.run(parent_state)

    assert observed == [wt]


def test_sequence_propagates_child_key() -> None:
    parent_state = _state()
    parent_state.child_key = "my-child"

    sub_state = _state()

    stage = SequenceStage("seq", body=[(sub_state, lambda: None)])
    stage.run(parent_state)

    assert sub_state.child_key == "my-child"


def test_sequence_propagates_session_dir() -> None:
    shard_dir = pathlib.Path("/tmp/shard-session")

    parent_state = _state()
    parent_state.session_dir = shard_dir

    sub_state = _state()
    assert sub_state.session_dir != shard_dir

    stage = SequenceStage("seq", body=[(sub_state, lambda: None)])
    stage.run(parent_state)

    assert sub_state.session_dir == shard_dir
