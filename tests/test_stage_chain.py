"""Tests for gremlins.stages.chain.Chain."""

from __future__ import annotations

import argparse
import json
import pathlib
from collections.abc import Callable
from typing import Any

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.clients import ClientSpec
from gremlins.pipeline import StageEntry
from gremlins.stages.base import StageContext
from gremlins.stages.chain import Chain

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_entry(options: dict[str, Any] | None = None) -> StageEntry:
    return StageEntry(
        name="chain",
        type="chain",
        prompt_paths=[],
        options=dict(options or {"child": "local"}),
        client=None,
    )


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    gr_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> StageContext:
    client = client or FakeClaudeClient()
    return StageContext(
        client=client,
        session_dir=tmp_path,
        gr_id=gr_id,
    )


def _make_chain(
    tmp_path: pathlib.Path,
    *,
    pipeline_builder: Callable | None = None,
    gr_id: str | None = None,
    client: FakeClaudeClient | None = None,
    options: dict[str, Any] | None = None,
) -> Chain:
    if pipeline_builder is None:

        def pipeline_builder(*_a: Any) -> list:
            return []

    entry = _make_entry(options)
    fake_client = client or FakeClaudeClient()
    chain = Chain(
        entry, ClientSpec("claude", "sonnet"), pipeline_builder=pipeline_builder
    )
    ctx = _make_ctx(tmp_path, gr_id=gr_id, client=fake_client)
    chain.bind(ctx)
    return chain


def _write_plan(tmp_path: pathlib.Path, text: str = "# Plan\n\nDo stuff.\n") -> None:
    (tmp_path / "plan.md").write_text(text, encoding="utf-8")


def _write_state(state_dir: pathlib.Path, gr_id: str, **extra: Any) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    data = {"id": gr_id, "stage": "", "bail_class": ""}
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
    # Also write the "out" handoff plan file
    out = session_dir / f"handoff-{n:03d}.md"
    out.write_text(f"# Updated plan {n}\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# _resolve_base_ref
# ---------------------------------------------------------------------------


def test_resolve_base_ref_returns_head_on_non_git(tmp_path, monkeypatch):
    chain = _make_chain(tmp_path)
    # Run in a non-git directory so rev-parse fails
    monkeypatch.chdir(tmp_path)
    ref = chain._resolve_base_ref()
    assert ref in ("HEAD", "") or len(ref) == 40  # either fallback or real SHA


# ---------------------------------------------------------------------------
# chain-done immediately (zero children)
# ---------------------------------------------------------------------------


def test_chain_done_immediately(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-chain-done-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    calls: list[str] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = int(str(args.out).split("-")[-1].split(".")[0])
        _make_signal_file(pathlib.Path(args.out).parent, n, "chain-done")
        calls.append("handoff")
        return 0

    monkeypatch.setattr("gremlins.stages.chain.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    chain = _make_chain(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(chain, "_resolve_base_ref", lambda: "abc123")
    chain.run(None)

    assert calls == ["handoff"]
    state = _read_state(state_dir)
    chain_st = state["chain_state"]
    assert chain_st["handoff_count"] == 1
    assert chain_st["current_child_stage"] is None


# ---------------------------------------------------------------------------
# chain runs N children, handoff terminates after N
# ---------------------------------------------------------------------------


def test_chain_runs_two_children(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-two-child-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    handoff_calls: list[int] = []
    child_runs: list[int] = []

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        n = handoff_calls.__len__() + 1
        handoff_calls.append(n)
        if n <= 2:
            # Write child plan
            child_plan = tmp_path / f"child-plan-{n}.md"
            child_plan.write_text(f"# Child {n}\n", encoding="utf-8")
            _make_signal_file(tmp_path, n, "next-plan", str(child_plan))
        else:
            _make_signal_file(tmp_path, n, "chain-done")
        return 0

    def fake_pipeline_builder(
        pipeline_name: str,
        plan_path: pathlib.Path,
        session_dir: pathlib.Path,
        resume_from: str | None,
    ) -> list[tuple[str, Callable[[], None]]]:
        child_runs.append(len(child_runs) + 1)
        return []  # no stages to run

    monkeypatch.setattr("gremlins.stages.chain.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    chain = _make_chain(tmp_path, gr_id=gr_id, pipeline_builder=fake_pipeline_builder)
    monkeypatch.setattr(chain, "_resolve_base_ref", lambda: "abc123")
    chain.run(None)

    assert len(handoff_calls) == 3
    assert len(child_runs) == 2

    state = _read_state(state_dir)
    chain_st = state["chain_state"]
    assert chain_st["handoff_count"] == 3
    assert chain_st["current_child_n"] == 2


# ---------------------------------------------------------------------------
# handoff bail propagates
# ---------------------------------------------------------------------------


def test_handoff_bail_raises(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-bail-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        _make_signal_file(tmp_path, 1, "bail", reason="scope too big")
        return 0

    monkeypatch.setattr("gremlins.stages.chain.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    chain = _make_chain(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(chain, "_resolve_base_ref", lambda: "abc123")

    with pytest.raises(RuntimeError, match="chain halted by handoff"):
        chain.run(None)

    state = _read_state(state_dir)
    assert state.get("bail_class") == "other"


# ---------------------------------------------------------------------------
# child bail propagates with bail_source="child"
# ---------------------------------------------------------------------------


def test_child_bail_propagates(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-child-bail-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id, bail_class="")
    _write_plan(tmp_path)

    child_plan = tmp_path / "child-plan-1.md"
    child_plan.write_text("# Child Plan\n", encoding="utf-8")

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        _make_signal_file(tmp_path, 1, "next-plan", str(child_plan))
        return 0

    def fake_pipeline_builder(
        pipeline_name: str,
        plan_path: pathlib.Path,
        session_dir: pathlib.Path,
        resume_from: str | None,
    ) -> list[tuple[str, Callable[[], None]]]:
        # Simulate a child stage that bails
        def _failing_stage() -> None:
            # Write bail_class into state before raising
            from gremlins.state import patch_state

            patch_state(gr_id, bail_class="other", bail_detail="child impl failed")
            raise RuntimeError("child bail")

        return [("implement", _failing_stage)]

    monkeypatch.setattr("gremlins.stages.chain.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    chain = _make_chain(tmp_path, gr_id=gr_id, pipeline_builder=fake_pipeline_builder)
    monkeypatch.setattr(chain, "_resolve_base_ref", lambda: "abc123")

    with pytest.raises(RuntimeError, match="child bail"):
        chain.run(None)

    state = _read_state(state_dir)
    assert state.get("bail_source") == "child"
    assert state.get("child_bail_class") == "other"


# ---------------------------------------------------------------------------
# resume mid-child: child_stage tracked and resumed
# ---------------------------------------------------------------------------


def test_resume_mid_child(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-resume-aabb12"
    state_dir = test_state_root / gr_id

    # Set up an existing state as if chain died mid-child
    child_dir = tmp_path / "child-001"
    child_dir.mkdir()
    (child_dir / "plan.md").write_text("# Child Plan\n", encoding="utf-8")

    chain_state = {
        "original_plan": str(tmp_path / "plan.md"),
        "base_ref": "abc123",
        "handoff_count": 1,
        "handoff_records": [
            {"n": 1, "exit_state": "next-plan", "signal_file": "", "plan_in": ""}
        ],
        "current_plan": str(tmp_path / "plan.md"),
        "current_child_n": 1,
        "current_child_stage": "implement",
    }
    _write_state(state_dir, gr_id, chain_state=chain_state)
    _write_plan(tmp_path)

    resumed_from: list[str | None] = []

    def fake_pipeline_builder(
        pipeline_name: str,
        plan_path: pathlib.Path,
        session_dir: pathlib.Path,
        resume_from: str | None,
    ) -> list[tuple[str, Callable[[], None]]]:
        resumed_from.append(resume_from)
        # Return a stub stage matching the resume_from stage so run_stages succeeds
        return [("implement", lambda: None)]

    def fake_handoff_run(client: Any, args: argparse.Namespace) -> int:
        # After child completes, declare done
        n = 2
        _make_signal_file(tmp_path, n, "chain-done")
        return 0

    monkeypatch.setattr("gremlins.stages.chain.handoff_mod.run", fake_handoff_run)
    monkeypatch.setenv("GR_ID", gr_id)

    chain = _make_chain(tmp_path, gr_id=gr_id, pipeline_builder=fake_pipeline_builder)
    chain.run(None)

    # Child was resumed from "implement"
    assert resumed_from == ["implement"]

    state = _read_state(state_dir)
    chain_st = state["chain_state"]
    assert chain_st["current_child_stage"] is None


# ---------------------------------------------------------------------------
# handoff agent failure raises
# ---------------------------------------------------------------------------


def test_handoff_nonzero_exit_raises(tmp_path, monkeypatch, test_state_root):
    gr_id = "boss-hfail-aabb12"
    state_dir = test_state_root / gr_id
    _write_state(state_dir, gr_id)
    _write_plan(tmp_path)

    monkeypatch.setattr("gremlins.stages.chain.handoff_mod.run", lambda *a, **kw: 1)
    monkeypatch.setenv("GR_ID", gr_id)

    chain = _make_chain(tmp_path, gr_id=gr_id)
    monkeypatch.setattr(chain, "_resolve_base_ref", lambda: "abc123")

    with pytest.raises(RuntimeError, match="handoff agent exited 1"):
        chain.run(None)


# ---------------------------------------------------------------------------
# boss.yaml loads and has expected stages
# ---------------------------------------------------------------------------


def test_boss_yaml_loads():
    from gremlins.pipeline import load_pipeline, resolve_pipeline_path

    pipeline = load_pipeline(resolve_pipeline_path("boss", pathlib.Path.cwd()))
    names = [s.name for s in pipeline.stages]
    assert names == ["chain", "review-chain", "address-chain"]
    types = [s.type for s in pipeline.stages]
    assert types == ["chain", "review-code", "address-code"]
    chain_entry = pipeline.stages[0]
    assert chain_entry.options.get("child") == "local"


# ---------------------------------------------------------------------------
# review-chain receives original plan (session_dir/plan.md)
# ---------------------------------------------------------------------------


def test_review_chain_reads_original_plan(tmp_path, monkeypatch, test_state_root):
    """The review-chain stage reads session_dir/plan.md, which is the original spec."""
    from gremlins.pipeline import load_pipeline, resolve_pipeline_path
    from gremlins.stages.base import StageContext
    from gremlins.stages.review_code import ReviewCode

    plan_text = "# Boss Plan\n\nOriginal spec content here.\n"
    plan_file = tmp_path / "plan.md"
    plan_file.write_text(plan_text, encoding="utf-8")

    prompts: list[str] = []

    class CapturingClient(FakeClaudeClient):
        def run(self, prompt: str, *, label: str, **kw: Any):
            prompts.append(prompt)
            # Write a review output file if path is in prompt
            import re

            m = re.search(r"`([^`]+\.md)`\s+is the canonical", prompt)
            if m:
                out = pathlib.Path(m.group(1))
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_text("# Review\n\nNo issues.\n")
            return super().run(prompt, label=label, **kw)

    pipeline = load_pipeline(resolve_pipeline_path("boss", tmp_path))
    review_entry = next(s for s in pipeline.stages if s.name == "review-chain")

    client = CapturingClient(
        fixtures={
            "review-chain:sonnet": [
                {"type": "system", "subtype": "init"},
                {"type": "result", "subtype": "success"},
            ]
        }
    )
    ctx = StageContext(client=client, session_dir=tmp_path, gr_id=None)
    stage = ReviewCode(review_entry, "sonnet", plan_text=plan_text, is_git=False)
    stage.bind(ctx)
    stage.run(None)

    assert prompts, "no prompts captured"
    assert "Original spec content here." in prompts[0]
