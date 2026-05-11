"""Tests for gremlins.stages.handoff.Handoff."""

from __future__ import annotations

import argparse
import json
import pathlib
from typing import Any

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.stages.handoff import Handoff
from gremlins.stages.loop import RunCmdFailed

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_state(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> RuntimeState:
    return RuntimeState(
        client=client or FakeClaudeClient(),
        session_dir=tmp_path,
        gr_id=gr_id,
    )


def _make_handoff(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> tuple[Handoff, RuntimeState]:
    fake_client = client or FakeClaudeClient()
    h = Handoff("handoff")
    state = _make_state(tmp_path, gr_id=gr_id, client=fake_client)
    return h, state


def _write_plan(tmp_path: pathlib.Path, text: str = "# Plan\n\nDo stuff.\n") -> None:
    (tmp_path / "plan.md").write_text(text, encoding="utf-8")


def _write_state(state_dir: pathlib.Path, gr_id: str, **extra: Any) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    data: dict[str, Any] = {"id": gr_id, "stage": "", "bail_class": ""}
    data.update(extra)
    (state_dir / "state.json").write_text(json.dumps(data), encoding="utf-8")


def _read_state(state_dir: pathlib.Path) -> dict[str, Any]:
    return json.loads((state_dir / "state.json").read_text(encoding="utf-8"))


def _make_signal_file(
    session_dir: pathlib.Path,
    n: int,
    exit_state: str,
    child_plan_path: str = "",
    reason: str = "",
) -> None:
    sig: dict[str, Any] = {"exit_state": exit_state}
    if child_plan_path:
        sig["child_plan"] = child_plan_path
    if reason:
        sig["reason"] = reason
    path = session_dir / f"handoff-{n:03d}.state.json"
    path.write_text(json.dumps(sig), encoding="utf-8")
    out = session_dir / f"handoff-{n:03d}.md"
    out.write_text(f"# Updated plan {n}\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# chain-done: returns normally, boss-spec.md restored to plan.md
# ---------------------------------------------------------------------------


def test_chain_done_immediately(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-done-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    calls: list[str] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        calls.append("handoff")
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda _state: "abc123")
    h.run(state)

    assert calls == ["handoff"]
    # boss-spec.md should have been created and plan.md restored from it
    assert (tmp_path / "boss-spec.md").exists()
    assert (tmp_path / "plan.md").read_text(encoding="utf-8") == "# Plan\n\nDo stuff.\n"


# ---------------------------------------------------------------------------
# next-plan: writes child plan to plan.md and raises RunCmdFailed
# ---------------------------------------------------------------------------


def test_next_plan_writes_plan_and_raises(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-nextplan-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    child_plan = tmp_path / "child-001.md"
    child_plan.write_text("# Child Plan\n", encoding="utf-8")

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(
            pathlib.Path(args.out).parent, n, "next-plan", str(child_plan)
        )
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda _state: "abc123")

    with pytest.raises(RunCmdFailed, match="next-plan"):
        h.run(state)

    assert (tmp_path / "plan.md").read_text(encoding="utf-8") == "# Child Plan\n"


# ---------------------------------------------------------------------------
# bail: emit_bail called, RuntimeError raised
# ---------------------------------------------------------------------------


def test_bail_emits_bail_and_raises(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-bail-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        _make_signal_file(tmp_path, 1, "bail", reason="scope too big")
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda _state: "abc123")

    with pytest.raises(RuntimeError, match="chain halted by handoff"):
        h.run(state)

    state_data = _read_state(state_dir)
    assert state_data.get("bail_class") == "other"


# ---------------------------------------------------------------------------
# first iteration: correct handoff index, boss-spec.md created
# ---------------------------------------------------------------------------


def test_handoff_index_first_iteration(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-persist-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    calls: list[int] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        calls.append(n)
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda _state: "abc123")
    h.run(state)

    assert calls == [1]
    assert (tmp_path / "boss-spec.md").exists()
    assert (tmp_path / "handoff-001.state.json").exists()


# ---------------------------------------------------------------------------
# handoff agent non-zero exit raises RuntimeError
# ---------------------------------------------------------------------------


def test_handoff_nonzero_exit_raises(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-hfail-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    monkeypatch.setattr("gremlins.stages.handoff.run", lambda *a, **kw: 1)
    monkeypatch.setenv("GR_ID", gr_id)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda _state: "abc123")

    with pytest.raises(RuntimeError, match="handoff agent exited 1"):
        h.run(state)


# ---------------------------------------------------------------------------
# resume: continues from file-based index (handoff-001.state.json → runs #2)
# ---------------------------------------------------------------------------


def test_resume_continues_from_file_index(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-resume-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)
    (tmp_path / "boss-spec.md").write_text("# Boss Spec\n", encoding="utf-8")
    # Simulate having already run one handoff (creates handoff-001.state.json and handoff-001.md)
    _make_signal_file(tmp_path, 1, "next-plan")

    calls: list[int] = []
    captured_plan: list[str] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        calls.append(n)
        captured_plan.append(args.plan)
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda _state: "abc123")
    h.run(state)

    # Should have run handoff #2 (index derived from existing handoff-001.state.json)
    assert calls == [2]
    # On resume, current_plan must be the previous rolling plan, not plan.md
    assert captured_plan == [str(tmp_path / "handoff-001.md")]


# ---------------------------------------------------------------------------
# base_ref_name from state: no git fallback needed when state has the field
# ---------------------------------------------------------------------------


def test_base_ref_from_state(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-baseref-aabb12"
    _write_plan(tmp_path)

    captured_base: list[str] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        captured_base.append(args.base)
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.run", fake_handoff_run)

    h, state = _make_handoff(tmp_path, gr_id=gr_id)
    state.base_ref_name = "deadbeef1234"
    # Do NOT monkeypatch _resolve_base_ref — state has base_ref_name, fallback must not run
    h.run(state)

    assert captured_base == ["deadbeef1234"]
