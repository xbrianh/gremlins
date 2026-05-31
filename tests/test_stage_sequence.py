"""Tests for SequenceStage."""

from __future__ import annotations

import asyncio
import json
import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome
from gremlins.stages.sequence import SequenceStage


def _state(**kw) -> RuntimeState:
    kw.setdefault("artifact_dir", pathlib.Path("/tmp"))
    return build_state(data=StateData(), client=FakeClaudeClient(), **kw)


class _FakeStage(Stage):
    """Minimal Stage that records the state it received and optionally raises."""

    def __init__(self, name: str, *, raises: Exception | None = None) -> None:
        super().__init__(name)
        self.received: RuntimeState | None = None
        self._raises = raises

    async def run(self, state: RuntimeState) -> Outcome:
        self.received = state
        if self._raises:
            raise self._raises
        return Done()


def test_sequence_runs_body_in_order() -> None:
    log: list[str] = []

    class _LogStage(Stage):
        def __init__(self, label: str) -> None:
            super().__init__(label)
            self._label = label

        async def run(self, state: RuntimeState) -> Outcome:
            log.append(self._label)
            return Done()

    stage = SequenceStage("seq", body=[_LogStage("a"), _LogStage("b"), _LogStage("c")])
    asyncio.run(stage.run(_state()))
    assert log == ["a", "b", "c"]


def test_sequence_stops_on_exception() -> None:
    log: list[str] = []

    class _LogStage(Stage):
        def __init__(self, label: str, *, fail: bool = False) -> None:
            super().__init__(label)
            self._label = label
            self._fail = fail

        async def run(self, state: RuntimeState) -> Outcome:
            log.append(self._label)
            if self._fail:
                raise RuntimeError("boom")
            return Done()

    stage = SequenceStage(
        "seq",
        body=[_LogStage("a"), _LogStage("b", fail=True), _LogStage("c")],
    )
    with pytest.raises(RuntimeError, match="boom"):
        asyncio.run(stage.run(_state()))
    assert log == ["a", "b"]


def test_sequence_propagates_worktree() -> None:
    wt = pathlib.Path("/tmp/fake-worktree")
    child = _FakeStage("child")
    stage = SequenceStage("seq", body=[child])
    asyncio.run(stage.run(_state(worktree=wt)))
    assert child.received is not None
    assert child.received.worktree == wt


def test_sequence_propagates_child_key() -> None:
    child = _FakeStage("child")
    stage = SequenceStage("seq", body=[child])
    asyncio.run(stage.run(_state(child_key="my-child")))
    assert child.received is not None
    assert child.received.child_key == "my-child"


def test_sequence_propagates_artifact_dir() -> None:
    shard_dir = pathlib.Path("/tmp/shard-session")
    child = _FakeStage("child")
    stage = SequenceStage("seq", body=[child])
    asyncio.run(stage.run(_state(artifact_dir=shard_dir)))
    assert child.received is not None
    assert child.received.artifact_dir == shard_dir


def _stateful(state_root: pathlib.Path, gremlin_id: str) -> RuntimeState:
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")
    return build_state(
        data=StateData(gremlin_id=gremlin_id, state_file=sf),
        client=FakeClaudeClient(),
        artifact_dir=state_dir,
    )


def test_sequence_resume_skips_completed_children(sandbox) -> None:
    ran: list[str] = []
    fail = {"b": True}

    class _TrackedStage(Stage):
        def __init__(self, label: str) -> None:
            super().__init__(label)

        async def run(self, s: RuntimeState) -> Outcome:
            ran.append(self.name)
            if self.name == "b" and fail["b"]:
                raise Bail("b failed")
            return Done()

    state = _stateful(sandbox.state, "gr-seq-resume")
    seq = SequenceStage(
        "seq", body=[_TrackedStage("a"), _TrackedStage("b"), _TrackedStage("c")]
    )

    with pytest.raises(Bail, match="b failed"):
        asyncio.run(seq.run(state))
    assert ran == ["a", "b"]

    ran.clear()
    fail["b"] = False

    asyncio.run(seq.run(state))
    assert ran == ["b", "c"]


def test_sibling_sequences_done_sets_are_independent(sandbox) -> None:
    ran: list[str] = []

    class _LogStage(Stage):
        def __init__(self, label: str) -> None:
            super().__init__(label)

        async def run(self, s: RuntimeState) -> Outcome:
            ran.append(self.name)
            return Done()

    state = _stateful(sandbox.state, "gr-seq-siblings")
    seq1 = SequenceStage("seq1", body=[_LogStage("a")])
    seq2 = SequenceStage("seq2", body=[_LogStage("a")])
    seq1.path = "pipeline/seq1"
    seq2.path = "pipeline/seq2"

    # Mark "a" done in seq1's slot only.
    state.mark_done("pipeline/seq1", "a")

    asyncio.run(seq1.run(state))
    asyncio.run(seq2.run(state))

    # seq1/a was skipped; seq2/a was not.
    assert ran == ["a"]  # only seq2's "a"
