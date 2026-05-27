"""Tests for gremlins.stages.handoff.Handoff."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import pathlib
from typing import Any

import pytest

import gremlins.executor.state as state_mod
from gremlins.artifacts.engine import EngineContext
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.agent import Agent
from gremlins.stages.handoff import Handoff
from gremlins.stages.outcome import Bail, Done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_state(
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> RuntimeState:
    return build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=client or FakeClaudeClient(),
        session_dir=tmp_path,
        artifacts=ArtifactRegistry(tmp_path),
    )


def _make_handoff(
    tmp_path: pathlib.Path,
    *,
    gremlin_id: str | None = None,
    client: FakeClaudeClient | None = None,
) -> tuple[Handoff, RuntimeState]:
    fake_client = client or FakeClaudeClient()
    h = Handoff("handoff")
    state = _make_state(tmp_path, gremlin_id=gremlin_id, client=fake_client)
    return h, state


def _write_plan(tmp_path: pathlib.Path, text: str = "# Plan\n\nDo stuff.\n") -> None:
    (tmp_path / "plan.md").write_text(text, encoding="utf-8")


def _write_state(state_dir: pathlib.Path, gremlin_id: str, **extra: Any) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    data: dict[str, Any] = {"id": gremlin_id, "stage": ""}
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


def _patch_handoff(monkeypatch: Any, tmp_path: pathlib.Path, exit_state: str) -> None:
    """Patch Agent.run and collect_git_context for a standard handoff test."""

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        for key in self.out_map:
            if key.startswith("handoff-"):
                n = int(key.split("-")[1])
                _make_signal_file(state.session_dir, n, exit_state)
        return Done()

    monkeypatch.setattr(Agent, "run", fake_agent_run)


# ---------------------------------------------------------------------------
# chain-done: returns normally, boss-spec.md restored to plan.md
# ---------------------------------------------------------------------------


def test_chain_done_immediately(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-done-aabb12"
    state_dir = sandbox.state / gremlin_id
    _write_state(state_dir, gremlin_id)
    _write_plan(tmp_path)

    _patch_handoff(monkeypatch, tmp_path, "chain-done")
    monkeypatch.setenv("GREMLIN_ID", gremlin_id)

    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)

    async def _fake_resolve_base_ref(_state: Any) -> str:
        return "abc123"

    monkeypatch.setattr(h, "_resolve_base_ref", _fake_resolve_base_ref)
    asyncio.run(h.run(state))

    assert (tmp_path / "boss-spec.md").exists()
    assert (tmp_path / "plan.md").read_text(encoding="utf-8") == "# Plan\n\nDo stuff.\n"


# ---------------------------------------------------------------------------
# next-plan: writes child plan to plan.md and returns NeedsFix
# ---------------------------------------------------------------------------


def test_next_plan_writes_plan_and_raises(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-nextplan-aabb12"
    state_dir = sandbox.state / gremlin_id
    _write_state(state_dir, gremlin_id)
    _write_plan(tmp_path)

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        for key in self.out_map:
            if key.startswith("handoff-"):
                n = int(key.split("-")[1])
                child = state.session_dir / f"handoff-{n:03d}-child.md"
                child.write_text("# Child Plan\n", encoding="utf-8")
                _make_signal_file(state.session_dir, n, "next-plan", str(child))
        return Done()

    monkeypatch.setattr(Agent, "run", fake_agent_run)
    monkeypatch.setenv("GREMLIN_ID", gremlin_id)

    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)

    async def _fake_resolve_base_ref(_state: Any) -> str:
        return "abc123"

    monkeypatch.setattr(h, "_resolve_base_ref", _fake_resolve_base_ref)

    outcome = asyncio.run(h.run(state))
    assert isinstance(outcome, Done)
    assert state.artifacts.produced("status")
    assert state.artifacts.read("status").strip() == b"needs_fix"
    assert (tmp_path / "plan.md").read_text(encoding="utf-8") == "# Child Plan\n"


# ---------------------------------------------------------------------------
# bail: emit_bail called, RuntimeError raised
# ---------------------------------------------------------------------------


def test_bail_emits_bail_and_raises(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-bail-aabb12"
    attempt = "handoff-test-attempt"
    state_dir = sandbox.state / gremlin_id
    _write_state(state_dir, gremlin_id)
    state_mod.StateData.load(gremlin_id).patch(attempt=attempt)
    _write_plan(tmp_path)

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        for key in self.out_map:
            if key.startswith("handoff-"):
                n = int(key.split("-")[1])
                _make_signal_file(state.session_dir, n, "bail", reason="scope too big")
        return Done()

    monkeypatch.setattr(Agent, "run", fake_agent_run)
    monkeypatch.setenv("GREMLIN_ID", gremlin_id)

    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)
    state.data.attempt = attempt

    async def _fake_resolve_base_ref(_state: Any) -> str:
        return "abc123"

    monkeypatch.setattr(h, "_resolve_base_ref", _fake_resolve_base_ref)

    with pytest.raises(Bail) as exc_info:
        asyncio.run(h.run(state))
    assert "chain halted by handoff" in exc_info.value.reason

    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    bail_data = json.loads(bail_file.read_text())
    assert bail_data["class"] == "other"


# ---------------------------------------------------------------------------
# first iteration: correct handoff index, boss-spec.md created
# ---------------------------------------------------------------------------


def test_handoff_index_first_iteration(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-persist-aabb12"
    state_dir = sandbox.state / gremlin_id
    _write_state(state_dir, gremlin_id)
    _write_plan(tmp_path)

    seen_keys: list[str] = []

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        for key in self.out_map:
            if key.startswith("handoff-"):
                seen_keys.append(key)
                n = int(key.split("-")[1])
                _make_signal_file(state.session_dir, n, "chain-done")
        return Done()

    monkeypatch.setattr(Agent, "run", fake_agent_run)
    monkeypatch.setenv("GREMLIN_ID", gremlin_id)

    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)

    async def _fake_resolve_base_ref(_state: Any) -> str:
        return "abc123"

    monkeypatch.setattr(h, "_resolve_base_ref", _fake_resolve_base_ref)
    asyncio.run(h.run(state))

    assert seen_keys == ["handoff-001"]
    assert (tmp_path / "boss-spec.md").exists()
    assert (tmp_path / "handoff-001.state.json").exists()


# ---------------------------------------------------------------------------
# handoff agent error raises RuntimeError
# ---------------------------------------------------------------------------


def test_handoff_agent_error_raises(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-hfail-aabb12"
    state_dir = sandbox.state / gremlin_id
    _write_state(state_dir, gremlin_id)
    _write_plan(tmp_path)

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        raise RuntimeError("agent exploded")

    monkeypatch.setattr(Agent, "run", fake_agent_run)
    monkeypatch.setenv("GREMLIN_ID", gremlin_id)

    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)

    async def _fake_resolve_base_ref(_state: Any) -> str:
        return "abc123"

    monkeypatch.setattr(h, "_resolve_base_ref", _fake_resolve_base_ref)

    with pytest.raises(RuntimeError, match="agent exploded"):
        asyncio.run(h.run(state))


# ---------------------------------------------------------------------------
# resume: continues from file-based index (handoff-001.state.json → runs #2)
# ---------------------------------------------------------------------------


def test_resume_continues_from_file_index(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-resume-aabb12"
    state_dir = sandbox.state / gremlin_id
    _write_state(state_dir, gremlin_id)
    _write_plan(tmp_path)
    (tmp_path / "boss-spec.md").write_text("# Boss Spec\n", encoding="utf-8")
    _make_signal_file(tmp_path, 1, "next-plan")

    seen_keys: list[str] = []
    captured_prompts: list[str] = []

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        captured_prompts.extend(self.prompts)
        for key in self.out_map:
            if key.startswith("handoff-"):
                seen_keys.append(key)
                n = int(key.split("-")[1])
                _make_signal_file(state.session_dir, n, "chain-done")
        return Done()

    monkeypatch.setattr(Agent, "run", fake_agent_run)
    monkeypatch.setenv("GREMLIN_ID", gremlin_id)

    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)

    async def _fake_resolve_base_ref(_state: Any) -> str:
        return "abc123"

    monkeypatch.setattr(h, "_resolve_base_ref", _fake_resolve_base_ref)
    asyncio.run(h.run(state))

    assert seen_keys == ["handoff-002"]
    # On resume the agent must receive the previous rolling plan, not plan.md
    rolling_plan_text = (tmp_path / "handoff-001.md").read_text(encoding="utf-8")
    assert captured_prompts and rolling_plan_text in captured_prompts[0]


# ---------------------------------------------------------------------------
# base_ref_name from state: no git fallback needed when state has the field
# ---------------------------------------------------------------------------


def test_base_ref_from_state(tmp_path, monkeypatch, sandbox):
    gremlin_id = "boss-handoff-baseref-aabb12"
    _write_plan(tmp_path)

    captured_base: list[str] = []

    async def fake_git_context(
        base_ref: str, rev: str | None = None
    ) -> tuple[str, str, str]:
        captured_base.append(base_ref)
        return "main", "", ""

    monkeypatch.setattr("gremlins.stages.handoff.collect_git_context", fake_git_context)

    async def fake_agent_run(self: Agent, state: RuntimeState) -> Done:
        for key in self.out_map:
            if key.startswith("handoff-"):
                n = int(key.split("-")[1])
                _make_signal_file(state.session_dir, n, "chain-done")
        return Done()

    monkeypatch.setattr(Agent, "run", fake_agent_run)

    engine_ctx = EngineContext(
        loop_iteration=1, attempt="", current_scope=(), base_ref="deadbeef1234"
    )
    h, state = _make_handoff(tmp_path, gremlin_id=gremlin_id)
    state = dataclasses.replace(state, engine_ctx=engine_ctx)
    asyncio.run(h.run(state))

    assert captured_base == ["deadbeef1234"]
