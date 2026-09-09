"""Tests for gremlins.stages.agent.run_agent."""

from __future__ import annotations

import asyncio
import json
import pathlib

import pytest
from _gremlins_core.stages import Bail
from conftest import MINIMAL_EVENTS

from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.agent_runner import run_agent
from tests.fake_client import FakeClient


def _make_state(
    tmp_path: pathlib.Path,
    *,
    attempt: str = "",
    fixtures: dict | None = None,
) -> RuntimeState:
    if fixtures is None:
        fixtures = {"test-label": MINIMAL_EVENTS}
    client = FakeClient(fixtures=fixtures)
    state_file: pathlib.Path | None = None
    data = StateData(gremlin_id=None)
    if attempt:
        state_dir = tmp_path / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        state_file = state_dir / "state.json"
        state_file.write_text(json.dumps({"id": "gr-test", "stage": ""}))
        data.state_file = state_file
        data.patch(attempt=attempt)
    return build_state(
        data=data,
        client=client,
        artifact_dir=tmp_path,
        worktree=tmp_path,
    )


def test_calls_client_run_with_expected_kwargs(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    asyncio.run(
        run_agent(state, "hello", label="test-label", raw_path=tmp_path / "out.jsonl")
    )

    assert len(state.client.calls) == 1
    call = state.client.calls[0]
    assert call.label == "test-label"
    assert call.prompt == "hello"
    assert call.cwd == tmp_path
    assert call.raw_path == tmp_path / "out.jsonl"
    assert call.model == state.client.model


def test_run_agent_with_attempt_set(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    asyncio.run(run_agent(state, "hello", label="test-label"))
    assert len(state.client.calls) == 1


def test_returns_completed_run_when_no_bail(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    result = asyncio.run(run_agent(state, "hello", label="test-label"))
    assert result.exit_code == 0


def test_raises_bail_when_transcript_has_sentinel(tmp_path):
    bail_events = [{"type": "result", "result": "work done\nBAIL: other: timed out"}]
    state = _make_state(tmp_path, fixtures={"test-label": bail_events})
    with pytest.raises(Bail) as exc_info:
        asyncio.run(run_agent(state, "hello", label="test-label"))
    assert "timed out" in exc_info.value.args[0]


def test_bail_detail_empty_when_sentinel_has_no_detail(tmp_path):
    bail_events = [{"type": "result", "result": "BAIL: other: "}]
    state = _make_state(tmp_path, fixtures={"test-label": bail_events})
    with pytest.raises(Bail) as exc_info:
        asyncio.run(run_agent(state, "hello", label="test-label"))
    assert exc_info.value.args[0] == ""


def test_no_bail_when_transcript_has_no_sentinel(tmp_path):
    state = _make_state(tmp_path)
    result = asyncio.run(run_agent(state, "hello", label="test-label"))
    assert result.exit_code == 0


def test_raises_bail_when_sentinel_present_without_attempt(tmp_path):
    bail_events = [{"type": "result", "result": "BAIL: other: reason"}]
    state = _make_state(tmp_path, fixtures={"test-label": bail_events})
    with pytest.raises(Bail):
        asyncio.run(run_agent(state, "hello", label="test-label"))


def test_model_kwarg_forwarded(tmp_path):
    state = _make_state(tmp_path)
    asyncio.run(run_agent(state, "hello", label="test-label", model="haiku"))
    assert state.client.calls[0].model == "haiku"


def test_token_usage_accumulated_into_state(tmp_path):
    usage_events = [
        {
            "type": "result",
            "result": "done",
            "token_usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cached_input_tokens": 90,
                "cache_creation_input_tokens": 5,
                "reasoning_tokens": 8,
                "turns": 3,
            },
        }
    ]
    state = _make_state(tmp_path, attempt="att1", fixtures={"test-label": usage_events})
    asyncio.run(run_agent(state, "hello", label="test-label"))
    asyncio.run(run_agent(state, "hello", label="test-label"))

    data = json.loads(state.data.state_file.read_text())
    assert data["token_usage"] == {
        "prompt_tokens": 200,
        "completion_tokens": 40,
        "cached_input_tokens": 180,
        "cache_creation_input_tokens": 10,
        "reasoning_tokens": 16,
        "turns": 6,
    }


def test_token_usage_absent_is_noop(tmp_path):
    state = _make_state(tmp_path, attempt="att1")
    asyncio.run(run_agent(state, "hello", label="test-label"))
    data = json.loads(state.data.state_file.read_text())
    assert "token_usage" not in data
