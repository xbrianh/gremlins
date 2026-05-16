"""Tests for LoopStage termination paths and RunCmd stage."""

from __future__ import annotations

import json
from typing import Any

from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Bail, Done, NeedsFix
from gremlins.stages.run_cmd import RunCmd


def _fake_client() -> Any:
    from gremlins.clients.fake import FakeClaudeClient

    return FakeClaudeClient(fixtures={})


def _loop_state(tmp_path: Any) -> RuntimeState:
    return RuntimeState(
        data=StateData(),
        client=_fake_client(),
        session_dir=tmp_path,
        worktree=tmp_path,
    )


# ---------------------------------------------------------------------------
# LoopStage termination paths
# ---------------------------------------------------------------------------


def test_loop_head_stable_exits_cleanly(tmp_path):
    calls: list[str] = []

    def runner() -> Done:
        calls.append("run")
        return Done()

    loop = LoopStage.from_runners([runner], max_iterations=3)
    outcome = loop.run(_loop_state(tmp_path))

    assert outcome == Done()
    assert calls == ["run"]


def test_loop_cmd_failure_then_fix_then_green(tmp_path):
    """NeedsFix on iter 1 allows fix runner; clean on iter 2."""
    state = {"attempt": 0, "fixed": False}

    def check() -> Done | NeedsFix:
        state["attempt"] += 1
        if not state["fixed"]:
            return NeedsFix("commands failed")
        return Done()

    def fix() -> Done:
        state["fixed"] = True
        return Done()

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    loop.run(_loop_state(tmp_path))

    assert state["attempt"] == 2
    assert state["fixed"]


def test_loop_fix_skipped_on_success(tmp_path):
    """Fix runner must not execute when the check runner succeeds."""
    fix_calls: list[int] = []

    def check() -> Done:
        return Done()

    def fix() -> Done:
        fix_calls.append(1)
        return Done()

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    loop.run(_loop_state(tmp_path))

    assert fix_calls == []


def test_loop_exhausted_returns_bail(tmp_path):
    def check() -> NeedsFix:
        return NeedsFix("always fails")

    def fix() -> Done:
        return Done()

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    outcome = loop.run(_loop_state(tmp_path))

    assert isinstance(outcome, Bail)


def test_loop_fix_skipped_on_final_iteration(tmp_path):
    """Fix runner must not execute on the last failed attempt."""
    fix_calls: list[int] = []
    attempt = [0]

    def check() -> NeedsFix:
        attempt[0] += 1
        return NeedsFix("fail")

    def fix() -> Done:
        fix_calls.append(attempt[0])
        return Done()

    loop = LoopStage.from_runners([check, fix], max_iterations=3)
    outcome = loop.run(_loop_state(tmp_path))

    assert isinstance(outcome, Bail)
    # fix ran for iterations 1 and 2, NOT 3
    assert fix_calls == [1, 2]


def test_loop_bail_propagates_immediately(tmp_path):
    """Bail returned from a body runner propagates without continuing."""

    def bail_runner() -> Bail:
        return Bail("stage bailed: bail_class=other")

    loop = LoopStage.from_runners([bail_runner], max_iterations=3)
    outcome = loop.run(_loop_state(tmp_path))

    assert isinstance(outcome, Bail)
    assert "bail_class=other" in outcome.reason


def test_loop_exhausted_emits_bail_to_state(tmp_path, make_state_dir):
    import gremlins.executor.state as state_mod

    gremlin_id = "loop-test-gr"
    state_dir = make_state_dir(gremlin_id)
    attempt = "loop-test-attempt"
    state_mod.StateData.load(gremlin_id).patch(attempt=attempt)

    def check() -> NeedsFix:
        return NeedsFix("fail")

    def fix() -> Done:
        return Done()

    loop_state = RuntimeState(
        data=StateData(gremlin_id=gremlin_id, attempt=attempt),
        client=_fake_client(),
        session_dir=tmp_path,
        worktree=tmp_path,
    )
    loop = LoopStage.from_runners([check, fix], max_iterations=2)
    outcome = loop.run(loop_state)

    assert isinstance(outcome, Bail)
    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"


# ---------------------------------------------------------------------------
# RunCmd stage
# ---------------------------------------------------------------------------


def _run_cmd_stage(tmp_path: Any, cmds: list[str]) -> tuple[RunCmd, RuntimeState]:
    stage = RunCmd("run-cmd", None, [], {"cmds": cmds})
    state = RuntimeState(
        data=StateData(),
        client=_fake_client(),
        session_dir=tmp_path,
        worktree=tmp_path,
    )
    return stage, state


def test_run_cmd_success(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["true"])
    outcome = stage.run(state)
    assert outcome == Done()


def test_run_cmd_failure_returns_needs_fix(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["false"])
    outcome = stage.run(state)
    assert isinstance(outcome, NeedsFix)


def test_run_cmd_failure_writes_log(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["echo boom >&2; false"])
    outcome = stage.run(state)
    assert isinstance(outcome, NeedsFix)
    log = tmp_path / "run-cmd.log"
    assert log.exists()


def test_run_cmd_empty_cmds_is_noop(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, [])
    outcome = stage.run(state)
    assert outcome == Done()
    assert not (tmp_path / "run-cmd.log").exists()


def test_run_cmd_output_in_needs_fix(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["echo hello_output; false"])
    outcome = stage.run(state)
    assert isinstance(outcome, NeedsFix)
    assert "hello_output" in outcome.detail


# ---------------------------------------------------------------------------
# pr_stack: detach-to-prior-PR logic
# ---------------------------------------------------------------------------


def _loop_state_with_gr(tmp_path: Any, gremlin_id: str) -> RuntimeState:
    return RuntimeState(
        data=StateData(gremlin_id=gremlin_id),
        client=_fake_client(),
        session_dir=tmp_path,
        worktree=tmp_path,
    )


def test_pr_stack_detaches_to_prior_pr_branch(tmp_path, make_state_dir, monkeypatch):
    gremlin_id = "pr-stack-test"
    state_dir = make_state_dir(gremlin_id)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
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
        "test", body_runners=[lambda: Done()], max_iterations=1, pr_stack=True
    )
    loop.run(_loop_state_with_gr(tmp_path, gremlin_id))

    assert detach_calls == ["feat-abc"]


def test_pr_stack_skipped_when_no_prior_pr(tmp_path, make_state_dir, monkeypatch):
    gremlin_id = "pr-stack-noop-test"
    make_state_dir(gremlin_id)

    git_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: git_calls.append(branch),
    )

    loop = LoopStage(
        "test", body_runners=[lambda: Done()], max_iterations=1, pr_stack=True
    )
    loop.run(_loop_state_with_gr(tmp_path, gremlin_id))

    assert git_calls == []


def test_pr_stack_false_skips_detach(tmp_path, make_state_dir, monkeypatch):
    gremlin_id = "pr-stack-disabled"
    state_dir = make_state_dir(gremlin_id)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
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
        "test", body_runners=[lambda: Done()], max_iterations=1, pr_stack=False
    )
    loop.run(_loop_state_with_gr(tmp_path, gremlin_id))

    assert git_calls == []


# ---------------------------------------------------------------------------
# loop_iteration written to state.json
# ---------------------------------------------------------------------------


def test_loop_patches_loop_iteration_to_state(tmp_path, make_state_dir):
    gremlin_id = "iter-patch-test"
    state_dir = make_state_dir(gremlin_id)
    seen_iterations: list[int] = []

    def runner() -> NeedsFix:
        data = json.loads((state_dir / "state.json").read_text())
        seen_iterations.append(int(data.get("loop_iteration") or 0))
        return NeedsFix("keep going")

    loop_state = RuntimeState(
        data=StateData(gremlin_id=gremlin_id),
        client=_fake_client(),
        session_dir=tmp_path,
        worktree=tmp_path,
    )
    loop = LoopStage.from_runners([runner], max_iterations=3)
    outcome = loop.run(loop_state)

    assert isinstance(outcome, Bail)
    assert seen_iterations == [1, 2, 3]


def test_pr_stack_iter2_detaches_to_iter1_branch(tmp_path, make_state_dir, monkeypatch):
    """Detach fires at start of iter2 using the artifact written during iter1."""
    gremlin_id = "pr-stack-two-iter"
    make_state_dir(gremlin_id)

    detach_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: detach_calls.append(branch),
    )

    count = 0

    def runner() -> Done | NeedsFix:
        nonlocal count
        count += 1
        if count == 1:
            from gremlins.executor.state import StateData

            StateData.load(gremlin_id).append_artifact(
                {
                    "type": "pr",
                    "url": "https://github.com/x/r/pull/1",
                    "branch": "feat-iter1",
                },
            )
            return NeedsFix("next-plan")
        return Done()

    loop = LoopStage("test", body_runners=[runner], max_iterations=2, pr_stack=True)
    loop.run(_loop_state_with_gr(tmp_path, gremlin_id))

    assert detach_calls == ["feat-iter1"]
