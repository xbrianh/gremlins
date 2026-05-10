"""Tests for SequenceStage."""

from __future__ import annotations

import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.stages.base import StageContext
from gremlins.stages.sequence import SequenceStage


def _ctx() -> StageContext:
    return StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
    )


def _make_sequence(runners: list) -> SequenceStage:
    body = [(_ctx(), fn) for fn in runners]
    stage = SequenceStage("seq", body=body)
    stage.bind(_ctx())
    return stage


def test_sequence_runs_body_in_order() -> None:
    log: list[str] = []
    stage = _make_sequence(
        [lambda: log.append("a"), lambda: log.append("b"), lambda: log.append("c")]
    )
    stage.run(None)
    assert log == ["a", "b", "c"]


def test_sequence_stops_on_exception() -> None:
    log: list[str] = []

    def fail() -> None:
        raise RuntimeError("boom")

    stage = _make_sequence([lambda: log.append("a"), fail, lambda: log.append("c")])
    with pytest.raises(RuntimeError, match="boom"):
        stage.run(None)
    assert log == ["a"]
    assert "c" not in log


def test_sequence_propagates_worktree() -> None:
    observed: list[pathlib.Path | None] = []
    wt = pathlib.Path("/tmp/fake-worktree")

    parent_ctx = _ctx()
    parent_ctx.worktree = wt

    sub_ctx = _ctx()

    def capture() -> None:
        observed.append(sub_ctx.worktree)

    stage = SequenceStage("seq", body=[(sub_ctx, capture)])
    stage.bind(parent_ctx)
    stage.run(None)

    assert observed == [wt]


def test_sequence_propagates_child_key() -> None:
    parent_ctx = _ctx()
    parent_ctx.child_key = "my-child"

    sub_ctx = _ctx()

    stage = SequenceStage("seq", body=[(sub_ctx, lambda: None)])
    stage.bind(parent_ctx)
    stage.run(None)

    assert sub_ctx.child_key == "my-child"


def test_sequence_propagates_session_dir() -> None:
    shard_dir = pathlib.Path("/tmp/shard-session")

    parent_ctx = _ctx()
    parent_ctx.session_dir = shard_dir

    sub_ctx = _ctx()
    assert sub_ctx.session_dir != shard_dir

    stage = SequenceStage("seq", body=[(sub_ctx, lambda: None)])
    stage.bind(parent_ctx)
    stage.run(None)

    assert sub_ctx.session_dir == shard_dir
