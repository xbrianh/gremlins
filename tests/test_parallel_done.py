"""Tests for per-child completion tracking (parallel_done) in parallel stages."""

from __future__ import annotations

import json
import pathlib

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData
from gremlins.stages.parallel import ParallelStage


@pytest.fixture
def state_root(tmp_path: pathlib.Path, monkeypatch):
    root = tmp_path / "state"
    monkeypatch.setattr("gremlins.paths.state_root", lambda: root)
    return root


def _make_state(state_root: pathlib.Path, gremlin_id: str) -> pathlib.Path:
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gremlin_id, "stage": ""}), encoding="utf-8")
    return sf


def _read_state(sf: pathlib.Path) -> dict:
    return json.loads(sf.read_text(encoding="utf-8"))


def _ctx(gremlin_id: str, sf: pathlib.Path, child_key: str) -> State:
    return State(
        data=StateData(gremlin_id=gremlin_id, state_file=sf),
        client=FakeClaudeClient(),
        session_dir=sf.parent,
        child_key=child_key,
    )


def _build_stages(group: str, runners: list, gremlin_id: str) -> list:
    return ParallelStage(group, []).build_runtime_stages(
        runners,
        parent_data=StateData.load(gremlin_id),
        project_root=pathlib.Path.cwd(),
    )


# ---------------------------------------------------------------------------
# Skip completed child on resume
# ---------------------------------------------------------------------------


def test_completed_child_skipped_on_resume(state_root):
    gremlin_id = "gr-done-skip"
    sf = _make_state(state_root, gremlin_id)

    ran: list[str] = []
    fail = {"b": True}

    def child_a() -> None:
        ran.append("a")

    def child_b() -> None:
        ran.append("b")
        if fail["b"]:
            raise RuntimeError("b failed")

    stages = _build_stages(
        "grp",
        [
            ("a", _ctx(gremlin_id, sf, "a"), child_a),
            ("b", _ctx(gremlin_id, sf, "b"), child_b),
        ],
        gremlin_id,
    )
    parallel_fn = stages[1][1]

    # First run: a succeeds, b fails.
    with pytest.raises(RuntimeError, match="b failed"):
        parallel_fn()

    state = _read_state(sf)
    assert "a" in state.get("done_children", {}).get("grp", [])
    assert "b" not in state.get("done_children", {}).get("grp", [])

    ran.clear()

    # Resume: only b should run.
    with pytest.raises(RuntimeError, match="b failed"):
        parallel_fn()

    assert ran == ["b"]


def test_both_children_present_in_done_after_second_run(state_root):
    gremlin_id = "gr-done-both"
    sf = _make_state(state_root, gremlin_id)

    ran: list[str] = []
    fail = {"b": True}

    def child_a() -> None:
        ran.append("a")

    def child_b() -> None:
        ran.append("b")
        if fail["b"]:
            raise RuntimeError("b failed")

    stages = _build_stages(
        "grp",
        [
            ("a", _ctx(gremlin_id, sf, "a"), child_a),
            ("b", _ctx(gremlin_id, sf, "b"), child_b),
        ],
        gremlin_id,
    )
    parallel_fn = stages[1][1]

    with pytest.raises(RuntimeError):
        parallel_fn()

    ran.clear()
    fail["b"] = False

    # Second run: b now succeeds; a is skipped.
    parallel_fn()

    assert ran == ["b"]
    state = _read_state(sf)
    done = state.get("done_children", {}).get("grp", [])
    assert "a" in done
    assert "b" in done


# ---------------------------------------------------------------------------
# parallel_done cleared after successful fan-in
# ---------------------------------------------------------------------------


def test_parallel_done_cleared_after_full_success(state_root):
    gremlin_id = "gr-done-clear"
    sf = _make_state(state_root, gremlin_id)

    stages = _build_stages(
        "grp",
        [
            ("a", _ctx(gremlin_id, sf, "a"), lambda: None),
            ("b", _ctx(gremlin_id, sf, "b"), lambda: None),
        ],
        gremlin_id,
    )
    parallel_fn = stages[1][1]
    fanin_fn = stages[2][1]

    parallel_fn()

    # done_children present before fan-in.
    assert "grp" in _read_state(sf).get("done_children", {})

    fanin_fn()

    # done_children absent after successful fan-in.
    assert "done_children" not in _read_state(sf)


# ---------------------------------------------------------------------------
# Bail aggregation still works when a done child bailed on a prior run
# ---------------------------------------------------------------------------


def test_bail_aggregation_unaffected_by_done_tracking(state_root):
    """Fan-in detects bail via parallel_attempts even when bailed child is in parallel_done."""
    gremlin_id = "gr-done-bail"
    sf = _make_state(state_root, gremlin_id)

    def child_bail() -> None:
        StateData.load(gremlin_id).patch_parallel_attempt("bail-child", "attempt-bail")
        bail_path = sf.parent / "bail_attempt-bail.json"
        bail_path.write_text(json.dumps({"class": "other", "detail": "nope"}))

    stages = _build_stages(
        "grp",
        [
            ("ok-child", _ctx(gremlin_id, sf, "ok-child"), lambda: None),
            ("bail-child", _ctx(gremlin_id, sf, "bail-child"), child_bail),
        ],
        gremlin_id,
    )
    parallel_fn = stages[1][1]
    fanin_fn = stages[2][1]

    parallel_fn()

    # ok-child is done; bail-child ran but wrote a bail file.
    state = _read_state(sf)
    assert "ok-child" in state.get("done_children", {}).get("grp", [])

    with pytest.raises(RuntimeError, match="bailed"):
        fanin_fn()
