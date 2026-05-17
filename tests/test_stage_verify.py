"""Tests for gremlins.stages.verify.Verify."""

from __future__ import annotations

import json
import pathlib
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.outcome import Bail
from gremlins.stages.verify import Verify

_VERIFY_PROMPT_PATH = (
    pathlib.Path(__file__).resolve().parent.parent
    / "gremlins"
    / "prompts"
    / "verify_fix.md"
)


def _make_stage(
    tmp_path: Any,
    *,
    cmds: list[str] | None = None,
    max_attempts: int = 3,
    client: Any = None,
) -> tuple[Verify, RuntimeState]:
    if cmds is None:
        cmds = ["true"]
    if client is None:
        client = FakeClaudeClient(fixtures={})
    options = {
        "cmds": cmds if cmds is not None else [],
        "max_attempts": max_attempts,
    }
    stage = Verify("verify", [_VERIFY_PROMPT_PATH.read_text(encoding="utf-8")], options)
    state = RuntimeState(
        data=StateData(),
        client=client,
        session_dir=tmp_path,
        worktree=tmp_path,
    )
    return stage, state


def test_green_on_first_attempt(tmp_path):
    stage, state = _make_stage(tmp_path, cmds=["true"])
    stage.run(state)
    assert len(state.client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_no_op_when_cmds_empty(tmp_path):
    """Empty cmds list -> stage skips without invoking the shell or agent."""
    stage, state = _make_stage(tmp_path, cmds=[])
    stage.run(state)
    assert len(state.client.calls) == 0
    assert not (tmp_path / "verify-attempt-1.log").exists()


def test_single_cmd(tmp_path):
    """A single cmd in the list runs without shell-syntax error."""
    stage, state = _make_stage(tmp_path, cmds=["true"])
    stage.run(state)
    assert len(state.client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_fix_then_green(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    check_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    stage, state = _make_stage(
        tmp_path,
        cmds=[check_cmd, "true"],
        max_attempts=3,
        client=client,
    )
    stage.run(state)

    assert len(state.client.calls) == 1
    assert state.client.calls[0].label == "verify-fix-1"
    assert (tmp_path / "verify-attempt-1.log").exists()
    assert (tmp_path / "verify-attempt-2.log").exists()
    assert (tmp_path / "stream-verify-1.jsonl").exists()


def test_attempts_exhausted_raises(tmp_path, monkeypatch):
    monkeypatch.delenv("GREMLIN_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    stage, state = _make_stage(
        tmp_path, cmds=["false", "true"], max_attempts=3, client=client
    )

    with pytest.raises(Bail):
        stage.run(state)

    assert len(state.client.calls) == 2
    assert (tmp_path / "verify-attempt-1.log").exists()
    assert (tmp_path / "verify-attempt-2.log").exists()
    assert (tmp_path / "verify-attempt-3.log").exists()


def test_exhaustion_with_max_1(tmp_path):
    stage, state = _make_stage(tmp_path, cmds=["false"], max_attempts=1)

    with pytest.raises(Bail):
        stage.run(state)

    assert len(state.client.calls) == 0
    assert (tmp_path / "verify-attempt-1.log").exists()


def test_both_cmds_in_fix_prompt(tmp_path, monkeypatch):
    monkeypatch.delenv("GREMLIN_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    stage, state = _make_stage(
        tmp_path,
        cmds=["false", "make test"],
        max_attempts=3,
        client=client,
    )

    with pytest.raises(Bail):
        stage.run(state)

    assert "false" in state.client.calls[0].prompt
    assert "make test" in state.client.calls[0].prompt


def test_log_file_captures_output(tmp_path):
    stage, state = _make_stage(tmp_path, cmds=["echo hello_check", "echo hello_test"])
    stage.run(state)

    log = tmp_path / "verify-attempt-1.log"
    assert log.exists()
    content = log.read_text()
    assert "hello_check" in content or "hello_test" in content


def test_no_pr_opened_on_exhaustion(tmp_path, monkeypatch):
    """Stage raises when all fix attempts are exhausted."""
    monkeypatch.delenv("GREMLIN_ID", raising=False)

    client = FakeClaudeClient(
        fixtures={
            "verify-fix-1": MINIMAL_EVENTS,
            "verify-fix-2": MINIMAL_EVENTS,
        }
    )
    stage, state = _make_stage(tmp_path, cmds=["false"], max_attempts=3, client=client)

    with pytest.raises(Bail):
        stage.run(state)

    assert len(state.client.calls) == 2


def test_exhaustion_emits_bail_to_state(tmp_path, make_state_dir):
    import gremlins.executor.state as state_mod

    gremlin_id = "test-gr-id"
    state_dir = make_state_dir(gremlin_id)
    attempt = "verify-exhaustion-attempt"
    state_mod.StateData.load(gremlin_id).patch(attempt=attempt)
    client = FakeClaudeClient(
        fixtures={"verify-fix-1": MINIMAL_EVENTS, "verify-fix-2": MINIMAL_EVENTS}
    )
    options = {"cmds": ["false"], "max_attempts": 3}
    stage = Verify("verify", [_VERIFY_PROMPT_PATH.read_text(encoding="utf-8")], options)
    state = RuntimeState(
        data=StateData(gremlin_id=gremlin_id, attempt=attempt),
        client=client,
        session_dir=tmp_path,
        worktree=tmp_path,
    )

    with pytest.raises(Bail):
        stage.run(state)

    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"


def test_commit_instr_in_fix_prompt(tmp_path):
    flag = tmp_path / "flag.txt"
    flag.write_text("fail\n")
    check_cmd = f"grep -q '^pass$' {flag}"

    class _FixingClient(FakeClaudeClient):
        def run(self, prompt, *, label, **kwargs):
            flag.write_text("pass\n")
            return super().run(prompt, label=label, **kwargs)

    client = _FixingClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    stage, state = _make_stage(tmp_path, cmds=[check_cmd], client=client)
    stage.run(state)

    assert len(state.client.calls) == 1
    assert "Fix failing checks" in state.client.calls[0].prompt
    assert "stage the changed files" in state.client.calls[0].prompt


def test_parallel_child_fix_prompt_uses_new_bail_command(tmp_path):
    client = FakeClaudeClient(fixtures={"verify-fix-1": MINIMAL_EVENTS})
    _bail_fix_path = (
        pathlib.Path(__file__).resolve().parent.parent
        / "gremlins"
        / "prompts"
        / "bail_section_fix.md"
    )
    prompts = [
        _VERIFY_PROMPT_PATH.read_text(encoding="utf-8"),
        _bail_fix_path.read_text(encoding="utf-8"),
    ]
    options = {"cmds": ["false"], "max_attempts": 2}
    stage = Verify("verify", prompts, options)
    state = RuntimeState(
        data=StateData(gremlin_id="gr-verify"),
        client=client,
        session_dir=tmp_path,
        child_key="verify-child",
        worktree=tmp_path,
    )

    with pytest.raises(Bail):
        stage.run(state)

    assert "python -c" in state.client.calls[0].prompt
    assert "gremlins.bail" not in state.client.calls[0].prompt
    assert "GREMLIN_STATE_DIR" in state.client.calls[0].prompt
