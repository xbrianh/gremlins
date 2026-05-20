"""Tests for active_children state tracking in container stages and fleet rendering."""

from __future__ import annotations

import asyncio
import json
import pathlib
from typing import Any

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData
from gremlins.fleet.render import build_row
from gremlins.fleet.views import _gremlin_to_json  # type: ignore[reportPrivateUsage]
from gremlins.stages.base import Stage
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Done, Outcome
from gremlins.stages.parallel import ParallelStage
from gremlins.stages.sequence import SequenceStage


def _stateful(tmp_path: pathlib.Path, gid: str = "test-id") -> State:
    sf = tmp_path / "state.json"
    sf.write_text(json.dumps({"id": gid}), encoding="utf-8")
    return State(
        data=StateData(gremlin_id=gid, state_file=sf),
        client=FakeClaudeClient(),
        session_dir=tmp_path,
    )


def _read_state(tmp_path: pathlib.Path) -> dict[str, Any]:
    return json.loads((tmp_path / "state.json").read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# Sequence
# ---------------------------------------------------------------------------


def test_sequence_active_children_cleared_after_run(tmp_path: pathlib.Path) -> None:
    state = _stateful(tmp_path)

    class _Spy(Stage):
        captured: list[str] | None = None

        async def run(self, state: State) -> Outcome:
            _Spy.captured = _read_state(tmp_path).get("active_children")
            return Done()

    seq = SequenceStage("seq", body=[_Spy("child-a")])
    asyncio.run(seq.run(state))

    assert _Spy.captured == ["child-a"]
    assert "active_children" not in _read_state(tmp_path)


def test_sequence_active_children_cleared_on_exception(tmp_path: pathlib.Path) -> None:
    state = _stateful(tmp_path)

    class _Boom(Stage):
        async def run(self, state: State) -> Outcome:
            raise RuntimeError("boom")

    seq = SequenceStage("seq", body=[_Boom("child-a")])
    with pytest.raises(RuntimeError, match="boom"):
        asyncio.run(seq.run(state))

    assert "active_children" not in _read_state(tmp_path)


# ---------------------------------------------------------------------------
# Loop
# ---------------------------------------------------------------------------


def test_loop_active_children_set_and_cleared(tmp_path: pathlib.Path) -> None:
    state = _stateful(tmp_path)
    captured: list[list[str] | None] = []

    class _Spy(Stage):
        async def run(self, state: State) -> Outcome:
            captured.append(_read_state(tmp_path).get("active_children"))
            return Done()

    loop = LoopStage("lp", body=[_Spy("body-stage")], max_iterations=1)
    asyncio.run(loop.run(state))

    assert captured == [["body-stage"]]
    assert "active_children" not in _read_state(tmp_path)


def test_loop_active_children_cleared_on_exception(tmp_path: pathlib.Path) -> None:
    state = _stateful(tmp_path)

    class _Boom(Stage):
        async def run(self, state: State) -> Outcome:
            raise RuntimeError("boom")

    loop = LoopStage("lp", body=[_Boom("body-stage")], max_iterations=1)
    with pytest.raises(RuntimeError, match="boom"):
        asyncio.run(loop.run(state))

    assert "active_children" not in _read_state(tmp_path)


# ---------------------------------------------------------------------------
# Render: build_row
# ---------------------------------------------------------------------------


def _row(state: dict[str, Any]) -> Any:
    return build_row(
        "gr-abc123", "/fake/state.json", "/tmp/fake-wdir", state, "running"
    )


def test_build_row_single_active_child() -> None:
    row = _row({"stage": "parallel", "active_children": ["review-pr"]})
    assert row.stage == "parallel/review-pr"


def test_build_row_multiple_active_children() -> None:
    row = _row({"stage": "parallel", "active_children": ["a", "b"]})
    assert row.stage == "parallel/[a,b]"


def test_build_row_no_active_children_when_waiting() -> None:
    row = _row({"stage": "waiting", "active_children": ["some-child"]})
    assert "some-child" not in row.stage
    assert row.stage.startswith("waiting")


def test_build_row_no_active_children_field() -> None:
    row = _row({"stage": "parallel"})
    assert row.stage == "parallel"


# ---------------------------------------------------------------------------
# JSON output: _gremlin_to_json
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Parallel
# ---------------------------------------------------------------------------


def _parallel_execute_stage(
    parent_data: StateData,
    child_fns: list[tuple[str, Any]],
    tmp_path: pathlib.Path,
) -> Any:
    child_runners = [
        (
            k,
            State(
                data=StateData(),
                client=FakeClaudeClient(),
                session_dir=tmp_path,
                child_key=k,
            ),
            fn,
        )
        for k, fn in child_fns
    ]
    stages = ParallelStage("grp", []).build_runtime_stages(
        child_runners, parent_data=parent_data, project_root=pathlib.Path.cwd()
    )
    return stages[1][1]  # execute stage


def test_parallel_active_children_set_and_cleared(tmp_path: pathlib.Path) -> None:
    sf = tmp_path / "state.json"
    sf.write_text(json.dumps({"id": "test-id"}), encoding="utf-8")
    parent_data = StateData(gremlin_id="test-id", state_file=sf)
    captured: list[list[str] | None] = []

    async def child_fn() -> None:
        captured.append(_read_state(tmp_path).get("active_children"))

    execute = _parallel_execute_stage(parent_data, [("child-a", child_fn)], tmp_path)
    asyncio.run(execute())

    assert captured == [["child-a"]]
    assert "active_children" not in _read_state(tmp_path)


def test_parallel_active_children_cleared_on_exception(tmp_path: pathlib.Path) -> None:
    sf = tmp_path / "state.json"
    sf.write_text(json.dumps({"id": "test-id"}), encoding="utf-8")
    parent_data = StateData(gremlin_id="test-id", state_file=sf)

    async def boom() -> None:
        raise RuntimeError("boom")

    execute = _parallel_execute_stage(parent_data, [("child-a", boom)], tmp_path)
    with pytest.raises(RuntimeError, match="boom"):
        asyncio.run(execute())

    assert "active_children" not in _read_state(tmp_path)
