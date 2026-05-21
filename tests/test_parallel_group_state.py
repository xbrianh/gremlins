"""Tests for ParallelGroupState: hydrate/persist round-trip, attempt recording, bail file shape."""

from __future__ import annotations

import json
import pathlib

import pytest

from gremlins.executor.parallel_state import ParallelGroupState
from gremlins.executor.state import StateData


@pytest.fixture
def state_root(tmp_path: pathlib.Path, monkeypatch):
    root = tmp_path / "state"
    monkeypatch.setattr("gremlins.paths.state_root", lambda: root)
    return root


def _make_state(state_root: pathlib.Path, gremlin_id: str) -> pathlib.Path:
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gremlin_id}), encoding="utf-8")
    return sf


def _read(sf: pathlib.Path) -> dict:
    return json.loads(sf.read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# Round-trip: persist → hydrate
# ---------------------------------------------------------------------------


def test_persist_and_hydrate_round_trip(tmp_path, state_root):
    gid = "gs-roundtrip"
    sf = _make_state(state_root, gid)

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.worktree_paths = {"a": tmp_path / "wt-a", "b": tmp_path / "wt-b"}
    gs.base_head = "abc123"
    gs.persist()

    raw = _read(sf)
    entry = raw["parallel_worktrees"]["reviews"]
    assert entry["base_head"] == "abc123"
    assert entry["paths"]["a"] == str(tmp_path / "wt-a")
    assert entry["paths"]["b"] == str(tmp_path / "wt-b")

    gs2 = ParallelGroupState("reviews", StateData.load(gid))
    gs2.hydrate()
    assert gs2.base_head == "abc123"
    assert gs2.worktree_paths["a"] == tmp_path / "wt-a"
    assert gs2.worktree_paths["b"] == tmp_path / "wt-b"


def test_hydrate_skips_when_paths_already_populated(tmp_path, state_root):
    gid = "gs-skip-hydrate"
    _make_state(state_root, gid)

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.worktree_paths["pre"] = tmp_path / "pre"
    gs.base_head = "original"

    # put different data in state.json
    data.patch_parallel_worktrees(
        "reviews", base_head="new-head", paths={"x": str(tmp_path / "x")}
    )

    gs.hydrate()  # should not overwrite
    assert gs.base_head == "original"
    assert "pre" in gs.worktree_paths
    assert "x" not in gs.worktree_paths


def test_clear_removes_group_entry(state_root):
    gid = "gs-clear"
    sf = _make_state(state_root, gid)

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.worktree_paths = {"a": pathlib.Path("/tmp/a")}
    gs.base_head = "sha1"
    gs.persist()

    assert "parallel_worktrees" in _read(sf)

    gs.clear()
    assert "parallel_worktrees" not in _read(sf)


def test_hydrate_missing_state_file_is_noop(tmp_path):
    data = StateData(gremlin_id=None)
    gs = ParallelGroupState("reviews", data)
    gs.hydrate()
    assert gs.worktree_paths == {}
    assert gs.base_head == ""


# ---------------------------------------------------------------------------
# Attempt recording
# ---------------------------------------------------------------------------


def test_record_attempt_writes_parallel_attempts(state_root):
    gid = "gs-attempt"
    sf = _make_state(state_root, gid)

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.record_attempt("child-a", "attempt-abc")

    raw = _read(sf)
    assert raw["parallel_attempts"]["child-a"] == "attempt-abc"


def test_clear_attempts_removes_parallel_attempts(state_root):
    gid = "gs-clear-attempts"
    sf = _make_state(state_root, gid)

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.record_attempt("child-a", "attempt-xyz")
    assert "parallel_attempts" in _read(sf)

    gs.clear_attempts()
    assert "parallel_attempts" not in _read(sf)


# ---------------------------------------------------------------------------
# Bail file shape
# ---------------------------------------------------------------------------


def test_write_bail_creates_bail_file_for_child(state_root):
    gid = "gs-bail"
    sf = _make_state(state_root, gid)
    state_dir = sf.parent

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.record_attempt("child-a", "attempt-bail")

    gs.write_bail("child-a", "something went wrong")

    bail_file = state_dir / "bail_attempt-bail.json"
    assert bail_file.exists()
    payload = json.loads(bail_file.read_text(encoding="utf-8"))
    assert payload["class"] == "other"
    assert payload["detail"] == "something went wrong"


def test_write_bail_no_attempt_is_noop(state_root):
    gid = "gs-bail-noop"
    sf = _make_state(state_root, gid)
    state_dir = sf.parent

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    # no attempt recorded for child-a
    gs.write_bail("child-a", "irrelevant")

    bail_files = list(state_dir.glob("bail_*.json"))
    assert not bail_files


# ---------------------------------------------------------------------------
# collect_bails
# ---------------------------------------------------------------------------


def test_collect_bails_returns_bailed_keys(state_root):
    gid = "gs-collect"
    sf = _make_state(state_root, gid)
    state_dir = sf.parent

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.record_attempt("child-a", "attempt-a")
    gs.record_attempt("child-b", "attempt-b")

    (state_dir / "bail_attempt-a.json").write_text(
        json.dumps({"class": "security", "detail": "oops"}), encoding="utf-8"
    )

    bailed, first_bail = gs.collect_bails(["child-a", "child-b"])
    assert bailed == ["child-a"]
    assert first_bail["class"] == "security"
    assert first_bail["detail"] == "oops"


def test_collect_bails_empty_when_no_bail_files(state_root):
    gid = "gs-collect-empty"
    _make_state(state_root, gid)

    data = StateData.load(gid)
    gs = ParallelGroupState("reviews", data)
    gs.record_attempt("child-a", "attempt-a")

    bailed, first_bail = gs.collect_bails(["child-a"])
    assert bailed == []
    assert first_bail == {}


def test_collect_bails_no_state_file_returns_empty():
    data = StateData(gremlin_id=None)
    gs = ParallelGroupState("reviews", data)
    bailed, first_bail = gs.collect_bails(["child-a"])
    assert bailed == []
    assert first_bail == {}
