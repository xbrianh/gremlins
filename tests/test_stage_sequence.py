"""Tests for SequenceStage."""

from __future__ import annotations

import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.stages.sequence import SequenceStage


def _state(**kw) -> RuntimeState:
    kw.setdefault("session_dir", pathlib.Path("/tmp"))
    return RuntimeState(data=StateData(), client=FakeClaudeClient(), **kw)


class _FakeStage(Stage):
    """Minimal Stage that records the state it received and optionally raises."""

    def __init__(self, name: str, *, raises: Exception | None = None) -> None:
        super().__init__(name, None, [], {})
        self.received: RuntimeState | None = None
        self._raises = raises

    def run(self, state: RuntimeState) -> Outcome:  # type: ignore[override]
        self.received = state
        if self._raises:
            raise self._raises
        return Done()


def test_sequence_runs_body_in_order() -> None:
    log: list[str] = []

    class _LogStage(Stage):
        def __init__(self, label: str) -> None:
            super().__init__(label, None, [], {})
            self._label = label

        def run(self, state: RuntimeState) -> Outcome:  # type: ignore[override]
            log.append(self._label)
            return Done()

    stage = SequenceStage("seq", body=[_LogStage("a"), _LogStage("b"), _LogStage("c")])
    stage.run(_state())
    assert log == ["a", "b", "c"]


def test_sequence_stops_on_exception() -> None:
    log: list[str] = []

    class _LogStage(Stage):
        def __init__(self, label: str, *, fail: bool = False) -> None:
            super().__init__(label, None, [], {})
            self._label = label
            self._fail = fail

        def run(self, state: RuntimeState) -> Outcome:  # type: ignore[override]
            log.append(self._label)
            if self._fail:
                raise RuntimeError("boom")
            return Done()

    stage = SequenceStage(
        "seq",
        body=[_LogStage("a"), _LogStage("b", fail=True), _LogStage("c")],
    )
    with pytest.raises(RuntimeError, match="boom"):
        stage.run(_state())
    assert log == ["a", "b"]


def test_sequence_propagates_worktree() -> None:
    wt = pathlib.Path("/tmp/fake-worktree")
    child = _FakeStage("child")
    stage = SequenceStage("seq", body=[child])
    stage.run(_state(worktree=wt))
    assert child.received is not None
    assert child.received.worktree == wt


def test_sequence_propagates_child_key() -> None:
    child = _FakeStage("child")
    stage = SequenceStage("seq", body=[child])
    stage.run(_state(child_key="my-child"))
    assert child.received is not None
    assert child.received.child_key == "my-child"


def test_sequence_propagates_session_dir() -> None:
    shard_dir = pathlib.Path("/tmp/shard-session")
    child = _FakeStage("child")
    stage = SequenceStage("seq", body=[child])
    stage.run(_state(session_dir=shard_dir))
    assert child.received is not None
    assert child.received.session_dir == shard_dir
