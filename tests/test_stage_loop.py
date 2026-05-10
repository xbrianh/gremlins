"""Tests for LoopStage termination paths and RunCmd stage."""

from __future__ import annotations

import json
from typing import Any

import pytest

from gremlins.stages import LoopExhausted, LoopStage, RunCmdFailed, StageContext
from gremlins.stages.run_cmd import RunCmd


def _fake_client() -> Any:
    from gremlins.clients.fake import FakeClaudeClient

    return FakeClaudeClient(fixtures={})


def _loop_ctx(tmp_path: Any) -> StageContext:
    return StageContext(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=None,
        worktree=tmp_path,
    )


# ---------------------------------------------------------------------------
# LoopStage termination paths
# ---------------------------------------------------------------------------


def test_loop_head_stable_exits_cleanly(tmp_path):
    calls: list[str] = []

    def runner() -> None:
        calls.append("run")

    loop = LoopStage.from_runners([runner], max_iterations=3)
    loop.bind(_loop_ctx(tmp_path))
    loop.run(None)

    assert calls == ["run"]


def test_loop_cmd_failure_then_fix_then_green(tmp_path):
    """RunCmdFailed on iter 1 allows fix runner; clean on iter 2."""
    state = {"attempt": 0, "fixed": False}

    def check() -> None:
        state["attempt"] += 1
        if not state["fixed"]:
            raise RunCmdFailed("commands failed")

    def fix() -> None:
        state["fixed"] = True

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    loop.bind(_loop_ctx(tmp_path))
    loop.run(None)

    assert state["attempt"] == 2
    assert state["fixed"]


def test_loop_fix_skipped_on_success(tmp_path):
    """Fix runner must not execute when the check runner succeeds."""
    fix_calls: list[int] = []

    def check() -> None:
        pass  # always succeeds

    def fix() -> None:
        fix_calls.append(1)

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    loop.bind(_loop_ctx(tmp_path))
    loop.run(None)

    assert fix_calls == []


def test_loop_exhausted_raises_loop_exhausted(tmp_path):
    def check() -> None:
        raise RunCmdFailed("always fails")

    def fix() -> None:
        pass

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    loop.bind(_loop_ctx(tmp_path))
    with pytest.raises(LoopExhausted):
        loop.run(None)


def test_loop_fix_skipped_on_final_iteration(tmp_path):
    """Fix runner must not execute on the last failed attempt."""
    fix_calls: list[int] = []
    attempt = [0]

    def check() -> None:
        attempt[0] += 1
        raise RunCmdFailed("fail")

    def fix() -> None:
        fix_calls.append(attempt[0])

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    loop.bind(_loop_ctx(tmp_path))
    with pytest.raises(LoopExhausted):
        loop.run(None)

    # fix ran for iterations 1 and 2, NOT 3
    assert fix_calls == [1, 2]


def test_loop_bail_propagates_immediately(tmp_path):
    """A RuntimeError (bail) from a body runner propagates without wrapping."""

    def bail_runner() -> None:
        raise RuntimeError("stage bailed: bail_class=other")

    loop = LoopStage.from_runners([bail_runner], max_iterations=3)
    loop.bind(_loop_ctx(tmp_path))
    with pytest.raises(RuntimeError, match="stage bailed"):
        loop.run(None)


def test_loop_exhausted_emits_bail_to_state(tmp_path, make_state_dir):
    gr_id = "loop-test-gr"
    state_dir = make_state_dir(gr_id)

    def check() -> None:
        raise RunCmdFailed("fail")

    def fix() -> None:
        pass

    ctx = StageContext(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=gr_id,
        worktree=tmp_path,
    )
    loop = LoopStage.from_runners([check, fix], max_iterations=2)
    loop.bind(ctx)
    with pytest.raises(LoopExhausted):
        loop.run(None)

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("bail_class") == "other"


# ---------------------------------------------------------------------------
# RunCmd stage
# ---------------------------------------------------------------------------


def _run_cmd_stage(tmp_path: Any, cmds: list[str]) -> RunCmd:
    stage = RunCmd("run-cmd", None, [], {"cmds": cmds})
    ctx = StageContext(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=None,
        worktree=tmp_path,
    )
    stage.bind(ctx)
    return stage


def test_run_cmd_success(tmp_path):
    stage = _run_cmd_stage(tmp_path, ["true"])
    stage.run(None)  # must not raise


def test_run_cmd_failure_raises_run_cmd_failed(tmp_path):
    stage = _run_cmd_stage(tmp_path, ["false"])
    with pytest.raises(RunCmdFailed):
        stage.run(None)


def test_run_cmd_failure_writes_log(tmp_path):
    stage = _run_cmd_stage(tmp_path, ["echo boom >&2; false"])
    with pytest.raises(RunCmdFailed):
        stage.run(None)
    log = tmp_path / "run-cmd.log"
    assert log.exists()


def test_run_cmd_empty_cmds_is_noop(tmp_path):
    stage = _run_cmd_stage(tmp_path, [])
    stage.run(None)  # must not raise, no log written
    assert not (tmp_path / "run-cmd.log").exists()


def test_run_cmd_output_in_exception(tmp_path):
    stage = _run_cmd_stage(tmp_path, ["echo hello_output; false"])
    with pytest.raises(RunCmdFailed) as exc_info:
        stage.run(None)
    assert "hello_output" in str(exc_info.value)
