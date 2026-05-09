"""Tests for gremlins.stages.handoff.Handoff."""

from __future__ import annotations

import argparse
import json
import pathlib
from typing import Any

import pytest

from gremlins.clients import ClientSpec
from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.handoff import Handoff
from gremlins.stages.loop import RunCmdFailed

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_entry() -> StageEntry:
    return StageEntry(
        name="handoff",
        type="handoff",
        prompt_paths=[],
        options={},
        client=None,
    )


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> StageContext:
    return StageContext(
        client=client or FakeClaudeClient(),
        session_dir=tmp_path,
        gr_id=gr_id,
    )


def _make_handoff(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> Handoff:
    entry = _make_entry()
    fake_client = client or FakeClaudeClient()
    h = Handoff(entry, ClientSpec("claude", "sonnet"))
    ctx = _make_ctx(tmp_path, gr_id=gr_id, client=fake_client)
    h.bind(ctx)
    return h


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

    monkeypatch.setattr("gremlins.stages.handoff.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda: "abc123")
    h.run(None)

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

    monkeypatch.setattr("gremlins.stages.handoff.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda: "abc123")

    with pytest.raises(RunCmdFailed, match="next-plan"):
        h.run(None)

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

    monkeypatch.setattr("gremlins.stages.handoff.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda: "abc123")

    with pytest.raises(RuntimeError, match="chain halted by handoff"):
        h.run(None)

    state = _read_state(state_dir)
    assert state.get("bail_class") == "other"


# ---------------------------------------------------------------------------
# chain_state persisted with correct handoff_count and records
# ---------------------------------------------------------------------------


def test_chain_state_persisted(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-persist-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda: "abc123")
    h.run(None)

    state = _read_state(state_dir)
    chain_st = state["chain_state"]
    assert chain_st["handoff_count"] == 1
    assert len(chain_st["handoff_records"]) == 1
    assert chain_st["handoff_records"][0]["exit_state"] == "chain-done"
    assert chain_st["base_ref"] == "abc123"


# ---------------------------------------------------------------------------
# handoff agent non-zero exit raises RuntimeError
# ---------------------------------------------------------------------------


def test_handoff_nonzero_exit_raises(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-hfail-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    monkeypatch.setattr("gremlins.stages.handoff.handoff_mod.run", lambda *a, **kw: 1)
    monkeypatch.setenv("GR_ID", gr_id)

    h = _make_handoff(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(h, "_resolve_base_ref", lambda: "abc123")

    with pytest.raises(RuntimeError, match="handoff agent exited 1"):
        h.run(None)


# ---------------------------------------------------------------------------
# resume with existing chain_state continues from handoff_count=1
# ---------------------------------------------------------------------------


def test_resume_with_existing_chain_state(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-handoff-resume-aabb12"
    state_dir = test_state_root / gr_id

    # Simulate having already run one handoff
    chain_state = {
        "original_plan": str(tmp_path / "boss-spec.md"),
        "base_ref": "abc123",
        "handoff_count": 1,
        "handoff_records": [
            {"n": 1, "exit_state": "next-plan", "signal_file": "", "plan_in": ""}
        ],
        "current_plan": str(tmp_path / "boss-spec.md"),
    }
    _write_state(state_dir, gr_id, chain_state=chain_state)
    _write_plan(tmp_path)
    (tmp_path / "boss-spec.md").write_text("# Boss Spec\n", encoding="utf-8")

    calls: list[int] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        calls.append(n)
        return 0

    monkeypatch.setattr("gremlins.stages.handoff.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    h = _make_handoff(tmp_path, gr_id=gr_id)
    h.run(None)

    # Should have run handoff #2 (continuing from count=1)
    assert calls == [2]
    state = _read_state(state_dir)
    chain_st = state["chain_state"]
    assert chain_st["handoff_count"] == 2
