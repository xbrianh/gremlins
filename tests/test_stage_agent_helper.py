"""Tests for gremlins.stages.agent.run_agent."""

from __future__ import annotations

import asyncio
import json
import pathlib

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.agent import run_agent
from gremlins.stages.outcome import Bail


def _make_state(
    tmp_path: pathlib.Path,
    *,
    attempt: str = "",
    fixtures: dict | None = None,
) -> RuntimeState:
    if fixtures is None:
        fixtures = {"test-label": MINIMAL_EVENTS}
    client = FakeClaudeClient(fixtures=fixtures)
    state_file: pathlib.Path | None = None
    if attempt:
        state_dir = tmp_path / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        state_file = state_dir / "state.json"
        state_file.write_text(json.dumps({"id": "gr-test", "stage": ""}))
    return RuntimeState(
        data=StateData(attempt=attempt, state_file=state_file),
        client=client,
        session_dir=tmp_path,
        worktree=tmp_path,
    )


def test_calls_client_run_with_expected_kwargs(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    asyncio.run(run_agent(state, "hello", label="test-label", raw_path=tmp_path / "out.jsonl"))

    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "test-label"
    assert call.prompt == "hello"
    assert call.cwd == tmp_path
    assert call.raw_path == tmp_path / "out.jsonl"
    assert call.model == state.client.model


def test_extra_env_injected_when_attempt_set(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    # FakeClaudeClient discards extra_env, but run_agent should compute it
    # without error and forward it.
    asyncio.run(run_agent(state, "hello", label="test-label"))
    assert len(state.client.calls) == 1


def test_returns_completed_run_when_no_bail(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    result = asyncio.run(run_agent(state, "hello", label="test-label"))
    assert result.exit_code == 0


def test_raises_bail_when_bail_file_exists(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    bail_path = state.data.state_file.parent / "bail_att1.json"
    bail_path.write_text(
        json.dumps({"class": "other", "detail": "timed out"}), encoding="utf-8"
    )
    with pytest.raises(Bail) as exc_info:
        asyncio.run(run_agent(state, "hello", label="test-label"))
    assert "timed out" in exc_info.value.args[0]


def test_bail_detail_empty_when_missing_field(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    bail_path = state.data.state_file.parent / "bail_att1.json"
    bail_path.write_text(json.dumps({"class": "other"}), encoding="utf-8")
    with pytest.raises(Bail) as exc_info:
        asyncio.run(run_agent(state, "hello", label="test-label"))
    assert exc_info.value.args[0] == ""


def test_no_bail_check_when_no_attempt(tmp_path):
    state = _make_state(tmp_path)
    result = asyncio.run(run_agent(state, "hello", label="test-label"))
    assert result.exit_code == 0


def test_model_kwarg_forwarded(tmp_path):
    state = _make_state(tmp_path)
    asyncio.run(run_agent(state, "hello", label="test-label", model="haiku"))
    assert state.client.calls[0].model == "haiku"


def test_stage_model_forwarded_when_set(tmp_path):
    """state.stage_model is used as the model when no explicit model= is given."""
    client = FakeClaudeClient(fixtures={"test-label": MINIMAL_EVENTS})
    state = RuntimeState(
        data=StateData(),
        client=client,
        session_dir=tmp_path,
        stage_model="sonnet",
    )
    asyncio.run(run_agent(state, "hello", label="test-label"))
    assert state.client.calls[0].model == "sonnet"


def test_stage_model_overridden_by_kwarg(tmp_path):
    """Explicit model= takes precedence over state.stage_model."""
    client = FakeClaudeClient(fixtures={"test-label": MINIMAL_EVENTS})
    state = RuntimeState(
        data=StateData(),
        client=client,
        session_dir=tmp_path,
        stage_model="sonnet",
    )
    asyncio.run(run_agent(state, "hello", label="test-label", model="haiku"))
    assert state.client.calls[0].model == "haiku"
