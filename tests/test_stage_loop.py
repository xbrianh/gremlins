"""Tests for LoopStage termination paths and RunCmd stage."""

from __future__ import annotations

import json
from typing import Any

import pytest

from gremlins.executor.state import State as RuntimeState
from gremlins.stages.loop import LoopExhausted, LoopStage, RunCmdFailed
from gremlins.stages.run_cmd import RunCmd


def _fake_client() -> Any:
    from gremlins.clients.fake import FakeClaudeClient

    return FakeClaudeClient(fixtures={})


def _loop_state(tmp_path: Any) -> RuntimeState:
    return RuntimeState(
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
    loop.run(_loop_state(tmp_path))

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
    loop.run(_loop_state(tmp_path))

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
    loop.run(_loop_state(tmp_path))

    assert fix_calls == []


def test_loop_exhausted_raises_loop_exhausted(tmp_path):
    def check() -> None:
        raise RunCmdFailed("always fails")

    def fix() -> None:
        pass

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    with pytest.raises(LoopExhausted):
        loop.run(_loop_state(tmp_path))


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
    with pytest.raises(LoopExhausted):
        loop.run(_loop_state(tmp_path))

    # fix ran for iterations 1 and 2, NOT 3
    assert fix_calls == [1, 2]


def test_loop_bail_propagates_immediately(tmp_path):
    """A RuntimeError (bail) from a body runner propagates without wrapping."""

    def bail_runner() -> None:
        raise RuntimeError("stage bailed: bail_class=other")

    loop = LoopStage.from_runners([bail_runner], max_iterations=3)
    with pytest.raises(RuntimeError, match="stage bailed"):
        loop.run(_loop_state(tmp_path))


def test_loop_exhausted_emits_bail_to_state(tmp_path, make_state_dir):
    gr_id = "loop-test-gr"
    state_dir = make_state_dir(gr_id)

    def check() -> None:
        raise RunCmdFailed("fail")

    def fix() -> None:
        pass

    loop_state = RuntimeState(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=gr_id,
        worktree=tmp_path,
    )
    loop = LoopStage.from_runners([check, fix], max_iterations=2)
    with pytest.raises(LoopExhausted):
        loop.run(loop_state)

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("bail_class") == "other"


# ---------------------------------------------------------------------------
# RunCmd stage
# ---------------------------------------------------------------------------


def _run_cmd_stage(tmp_path: Any, cmds: list[str]) -> tuple[RunCmd, RuntimeState]:
    stage = RunCmd("run-cmd", None, [], {"cmds": cmds})
    state = RuntimeState(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=None,
        worktree=tmp_path,
    )
    return stage, state


def test_run_cmd_success(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["true"])
    stage.run(state)  # must not raise


def test_run_cmd_failure_raises_run_cmd_failed(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["false"])
    with pytest.raises(RunCmdFailed):
        stage.run(state)


def test_run_cmd_failure_writes_log(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["echo boom >&2; false"])
    with pytest.raises(RunCmdFailed):
        stage.run(state)
    log = tmp_path / "run-cmd.log"
    assert log.exists()


def test_run_cmd_empty_cmds_is_noop(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, [])
    stage.run(state)  # must not raise, no log written
    assert not (tmp_path / "run-cmd.log").exists()


def test_run_cmd_output_in_exception(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["echo hello_output; false"])
    with pytest.raises(RunCmdFailed) as exc_info:
        stage.run(state)
    assert "hello_output" in str(exc_info.value)


# ---------------------------------------------------------------------------
# pr_stack: detach-to-prior-PR logic
# ---------------------------------------------------------------------------


def _loop_state_with_gr(tmp_path: Any, gr_id: str) -> RuntimeState:
    return RuntimeState(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=gr_id,
        worktree=tmp_path,
    )


def test_pr_stack_detaches_to_prior_pr_branch(tmp_path, make_state_dir, monkeypatch):
    gr_id = "pr-stack-test"
    state_dir = make_state_dir(gr_id)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "stage": "",
                "bail_class": "",
                "artifacts": [
                    {
                        "type": "pr",
                        "url": "https://github.com/x/r/pull/1",
                        "branch": "feat-abc",
                    }
                ],
            }
        )
    )

    detach_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: detach_calls.append(branch),
    )

    loop = LoopStage(
        "test", body_runners=[lambda: None], max_iterations=1, pr_stack=True
    )
    loop.run(_loop_state_with_gr(tmp_path, gr_id))

    assert detach_calls == ["feat-abc"]


def test_pr_stack_skipped_when_no_prior_pr(tmp_path, make_state_dir, monkeypatch):
    gr_id = "pr-stack-noop-test"
    make_state_dir(gr_id)

    git_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: git_calls.append(branch),
    )

    loop = LoopStage(
        "test", body_runners=[lambda: None], max_iterations=1, pr_stack=True
    )
    loop.run(_loop_state_with_gr(tmp_path, gr_id))

    assert git_calls == []


def test_pr_stack_false_skips_detach(tmp_path, make_state_dir, monkeypatch):
    gr_id = "pr-stack-disabled"
    state_dir = make_state_dir(gr_id)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "stage": "",
                "bail_class": "",
                "artifacts": [
                    {
                        "type": "pr",
                        "url": "https://github.com/x/r/pull/1",
                        "branch": "feat-xyz",
                    }
                ],
            }
        )
    )

    git_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: git_calls.append(branch),
    )

    loop = LoopStage(
        "test", body_runners=[lambda: None], max_iterations=1, pr_stack=False
    )
    loop.run(_loop_state_with_gr(tmp_path, gr_id))

    assert git_calls == []


# ---------------------------------------------------------------------------
# loop_iteration written to state.json
# ---------------------------------------------------------------------------


def test_loop_patches_loop_iteration_to_state(tmp_path, make_state_dir):
    gr_id = "iter-patch-test"
    state_dir = make_state_dir(gr_id)
    seen_iterations: list[int] = []

    def runner() -> None:
        data = json.loads((state_dir / "state.json").read_text())
        seen_iterations.append(int(data.get("loop_iteration") or 0))
        raise RunCmdFailed("keep going")

    loop_state = RuntimeState(
        client=_fake_client(),
        session_dir=tmp_path,
        gr_id=gr_id,
        worktree=tmp_path,
    )
    loop = LoopStage.from_runners([runner], max_iterations=3)
    with pytest.raises(LoopExhausted):
        loop.run(loop_state)

    assert seen_iterations == [1, 2, 3]


def test_pr_stack_iter2_detaches_to_iter1_branch(tmp_path, make_state_dir, monkeypatch):
    """Detach fires at start of iter2 using the artifact written during iter1."""
    gr_id = "pr-stack-two-iter"
    make_state_dir(gr_id)

    detach_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: detach_calls.append(branch),
    )

    count = 0

    def runner() -> None:
        nonlocal count
        count += 1
        if count == 1:
            from gremlins.executor.state import append_artifact

            append_artifact(
                gr_id,
                {
                    "type": "pr",
                    "url": "https://github.com/x/r/pull/1",
                    "branch": "feat-iter1",
                },
            )
            raise RunCmdFailed("next-plan")

    loop = LoopStage("test", body_runners=[runner], max_iterations=2, pr_stack=True)
    loop.run(_loop_state_with_gr(tmp_path, gr_id))

    assert detach_calls == ["feat-iter1"]
