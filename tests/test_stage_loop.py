"""Tests for LoopStage termination paths and Exec stage."""

from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest

from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.exec import Exec as Cmd
from gremlins.stages.loop import LoopStage, detach_to_pr_base, head_stable, max_iters
from gremlins.stages.outcome import Bail, Done


def _fake_client() -> Any:
    from gremlins.clients.fake import FakeClaudeClient

    return FakeClaudeClient(fixtures={})


def _loop_state(tmp_path: Any) -> RuntimeState:
    (tmp_path / "artifacts").mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=_fake_client(),
        session_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )


def _set_marker(state: RuntimeState) -> None:
    """Write the status=needs_fix marker artifact to signal loop failure."""
    state.artifacts.write("status", "needs_fix")


# ---------------------------------------------------------------------------
# LoopStage termination paths
# ---------------------------------------------------------------------------


def test_loop_head_stable_exits_cleanly(tmp_path):
    calls: list[str] = []

    async def runner() -> Done:
        calls.append("run")
        return Done()

    loop = LoopStage("loop", body_runners=[runner], max_iterations=3)
    outcome = asyncio.run(loop.run(_loop_state(tmp_path)))

    assert outcome == Done()
    assert calls == ["run"]


def test_loop_cmd_failure_then_fix_then_green(tmp_path):
    """Marker on iter 1 allows fix runner; clean on iter 2."""
    loop_state = _loop_state(tmp_path)
    attempt = {"attempt": 0, "fixed": False}

    async def check() -> Done:
        attempt["attempt"] += 1
        if not attempt["fixed"]:
            _set_marker(loop_state)
        return Done()

    async def fix() -> Done:
        attempt["fixed"] = True
        return Done()

    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=3)
    asyncio.run(loop.run(loop_state))

    assert attempt["attempt"] == 2
    assert attempt["fixed"]


def test_loop_fix_skipped_on_success(tmp_path):
    """Fix runner must not execute when the check runner succeeds."""
    fix_calls: list[int] = []

    async def check() -> Done:
        return Done()

    async def fix() -> Done:
        fix_calls.append(1)
        return Done()

    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=3)
    asyncio.run(loop.run(_loop_state(tmp_path)))

    assert fix_calls == []


def test_loop_exhausted_returns_bail(tmp_path):
    loop_state = _loop_state(tmp_path)

    async def check() -> Done:
        _set_marker(loop_state)
        return Done()

    async def fix() -> Done:
        return Done()

    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(loop_state))


def test_loop_fix_skipped_on_final_iteration(tmp_path):
    """Fix runner must not execute on the last failed attempt."""
    fix_calls: list[int] = []
    attempt = [0]
    loop_state = _loop_state(tmp_path)

    async def check() -> Done:
        attempt[0] += 1
        _set_marker(loop_state)
        return Done()

    async def fix() -> Done:
        fix_calls.append(attempt[0])
        return Done()

    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(loop_state))
    # fix ran for iterations 1 and 2, NOT 3
    assert fix_calls == [1, 2]


def test_loop_bail_propagates_immediately(tmp_path):
    """Bail raised from a body runner propagates without continuing."""

    async def bail_runner() -> Done:
        raise Bail("stage bailed: bail_class=other")

    loop = LoopStage("loop", body_runners=[bail_runner], max_iterations=3)
    with pytest.raises(Bail) as exc_info:
        asyncio.run(loop.run(_loop_state(tmp_path)))
    assert "bail_class=other" in exc_info.value.reason


def test_loop_exhausted_emits_bail_to_state(tmp_path, make_state_dir):
    import gremlins.executor.state as state_mod

    gremlin_id = "loop-test-gr"
    state_dir = make_state_dir(gremlin_id)
    attempt = "loop-test-attempt"
    state_mod.StateData.load(gremlin_id).patch(attempt=attempt)

    (tmp_path / "artifacts").mkdir(exist_ok=True)
    loop_state = build_state(
        data=StateData(gremlin_id=gremlin_id, attempt=attempt),
        client=_fake_client(),
        session_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )

    async def check() -> Done:
        _set_marker(loop_state)
        return Done()

    async def fix() -> Done:
        return Done()

    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=2)
    with pytest.raises(Bail):
        asyncio.run(loop.run(loop_state))

    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"


# ---------------------------------------------------------------------------
# Termination predicates
# ---------------------------------------------------------------------------


def test_head_stable_returns_true_when_head_unchanged(tmp_path):
    state = _loop_state(tmp_path)
    # tmp_path is not a git repo → head_sha returns ""; passing "" means stable
    assert head_stable(state, 1, "") is True


def test_head_stable_returns_false_when_head_changed(tmp_path):
    state = _loop_state(tmp_path)
    assert head_stable(state, 1, "old-sha-abc123") is False


def test_max_iters_terminates_at_n(tmp_path):
    state = _loop_state(tmp_path)
    pred = max_iters(2)
    assert not pred(state, 1, "")
    assert pred(state, 2, "")
    assert pred(state, 3, "")


def test_custom_until_predicate(tmp_path):
    """Loop exits when custom predicate returns True."""

    async def runner() -> Done:
        return Done()

    # max_iters(2) fires at iteration 2 — no Bail raised
    loop = LoopStage(
        "loop",
        body_runners=[runner],
        max_iterations=5,
        until=max_iters(2),
    )
    result = asyncio.run(loop.run(_loop_state(tmp_path)))
    assert result == Done()


def test_on_iteration_start_called_each_iteration(tmp_path):
    """on_iteration_start fires once per iteration."""
    calls: list[int] = []
    iter_counter = [0]
    loop_state = _loop_state(tmp_path)

    def on_start(state: RuntimeState) -> None:
        calls.append(1)

    async def runner() -> Done:
        iter_counter[0] += 1
        if iter_counter[0] < 2:
            _set_marker(loop_state)
        return Done()

    loop = LoopStage(
        "loop",
        body_runners=[runner],
        max_iterations=3,
        on_iteration_start=on_start,
    )
    asyncio.run(loop.run(loop_state))
    assert len(calls) == 2  # fired for iteration 1 and 2


# ---------------------------------------------------------------------------
# Exec stage
# ---------------------------------------------------------------------------


def _run_cmd_stage(tmp_path: Any, cmds: list[str]) -> tuple[Cmd, RuntimeState]:
    (tmp_path / "artifacts").mkdir(exist_ok=True)
    stage = Cmd("cmd", {"cmds": cmds})
    state = build_state(
        data=StateData(),
        client=_fake_client(),
        session_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )
    return stage, state


def test_run_cmd_success(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["true"])
    outcome = asyncio.run(stage.run(state))
    assert outcome == Done()


def test_run_cmd_failure_raises_bail(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["false"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))


def test_run_cmd_failure_writes_log(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["echo boom >&2; false"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))
    log = tmp_path / "artifacts" / "exec-cmd.log"
    assert log.exists()


def test_run_cmd_empty_cmds_is_noop(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, [])
    outcome = asyncio.run(stage.run(state))
    assert outcome == Done()
    assert not (tmp_path / "artifacts" / "exec-cmd.log").exists()


def test_run_cmd_output_written_to_log(tmp_path):
    stage, state = _run_cmd_stage(tmp_path, ["echo hello_output; false"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))
    log = tmp_path / "artifacts" / "exec-cmd.log"
    assert "hello_output" in log.read_text()


# ---------------------------------------------------------------------------
# on_iteration_start: replaces pr_stack detach-to-prior-PR logic
# ---------------------------------------------------------------------------


def _loop_state_with_gr(
    tmp_path: Any, gremlin_id: str, *, pr_branch: str | None = None
) -> RuntimeState:
    (tmp_path / "artifacts").mkdir(exist_ok=True)
    state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=_fake_client(),
        session_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )
    if pr_branch is not None:
        from gremlins.artifacts.uri import Uri

        branch_file = tmp_path / "artifacts" / "pr-branch.txt"
        branch_file.write_text(pr_branch)
        state.artifacts.bind("pr-branch", Uri.parse("file://session/pr-branch.txt"))
    return state


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

    async def done_runner() -> Done:
        return Done()

    loop = LoopStage(
        "test",
        body_runners=[done_runner],
        max_iterations=1,
        on_iteration_start=detach_to_pr_base,
    )
    asyncio.run(
        loop.run(_loop_state_with_gr(tmp_path, gremlin_id, pr_branch="feat-abc"))
    )

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

    async def done_runner() -> Done:
        return Done()

    loop = LoopStage(
        "test",
        body_runners=[done_runner],
        max_iterations=1,
        on_iteration_start=detach_to_pr_base,
    )
    asyncio.run(loop.run(_loop_state_with_gr(tmp_path, gremlin_id)))

    assert git_calls == []


def test_no_on_iteration_start_skips_detach(tmp_path, make_state_dir, monkeypatch):
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

    async def done_runner() -> Done:
        return Done()

    loop = LoopStage("test", body_runners=[done_runner], max_iterations=1)
    asyncio.run(loop.run(_loop_state_with_gr(tmp_path, gremlin_id)))

    assert git_calls == []


# ---------------------------------------------------------------------------
# loop_iteration written to state.json
# ---------------------------------------------------------------------------


def test_loop_patches_loop_iteration_to_state(tmp_path, make_state_dir):
    gremlin_id = "iter-patch-test"
    state_dir = make_state_dir(gremlin_id)
    seen_iterations: list[int] = []

    (tmp_path / "artifacts").mkdir(exist_ok=True)
    loop_state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=_fake_client(),
        session_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )

    async def runner() -> Done:
        data = json.loads((state_dir / "state.json").read_text())
        seen_iterations.append(int(data.get("loop_iteration") or 0))
        _set_marker(loop_state)
        return Done()

    loop = LoopStage("loop", body_runners=[runner], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(loop_state))

    assert seen_iterations == [1, 2, 3]


def test_loop_unbinds_out_keys_between_iterations(tmp_path):
    """out_map keys unbound each iteration so exec can rebind to a different URI."""
    from gremlins.stages.exec import Exec

    (tmp_path / "artifacts").mkdir(exist_ok=True)
    state = _loop_state(tmp_path)

    bound_count = [0]

    async def binder() -> Done:
        # Bind a different URI each iteration; without unbind the second bind raises DuplicateArtifact.
        uri = Uri.parse(f"file://session/out-{bound_count[0]}.txt")
        state.artifacts.bind("loop-out", uri)
        bound_count[0] += 1
        if bound_count[0] < 2:
            _set_marker(state)
        return Done()

    exec_stage = Exec("stage", {}, out_map={"loop-out": "file://session/out-0.txt"})
    loop = LoopStage(
        "loop",
        body=[exec_stage],
        body_runners=[binder],
        max_iterations=3,
    )
    asyncio.run(loop.run(state))
    assert bound_count[0] == 2


def test_pr_stack_unbind_fires_after_on_iteration_start(
    tmp_path, make_state_dir, monkeypatch
):
    """Unbind runs after on_iteration_start so detach_to_pr_base can read the pr artifact."""
    from gremlins.stages.exec import Exec

    gremlin_id = "pr-stack-order-test"
    make_state_dir(gremlin_id)

    detach_calls: list[str] = []
    from gremlins.stages import loop as _loop_mod

    monkeypatch.setattr(
        _loop_mod._git,
        "git_detach_to_branch",
        lambda branch, cwd=None: detach_calls.append(branch),
    )

    state = _loop_state_with_gr(tmp_path, gremlin_id)
    count = [0]

    async def runner() -> Done:
        count[0] += 1
        if count[0] == 1:
            from gremlins.artifacts.uri import Uri

            branch_file = tmp_path / "artifacts" / "pr-branch.txt"
            branch_file.write_text("feat-iter1")
            state.artifacts.bind("pr-branch", Uri.parse("file://session/pr-branch.txt"))
            _set_marker(state)
        return Done()

    exec_stage = Exec(
        "stage", {}, out_map={"pr-branch": "file://session/pr-branch.txt"}
    )
    loop = LoopStage(
        "test",
        body=[exec_stage],
        body_runners=[runner],
        max_iterations=2,
        on_iteration_start=detach_to_pr_base,
    )
    asyncio.run(loop.run(state))

    # on_iteration_start fires before unbind, so detach_to_pr_base sees pr-branch on iter 2
    assert detach_calls == ["feat-iter1"]


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

    state = _loop_state_with_gr(tmp_path, gremlin_id)
    count = 0

    async def runner() -> Done:
        nonlocal count
        count += 1
        if count == 1:
            from gremlins.artifacts.uri import Uri

            branch_file = tmp_path / "artifacts" / "pr-branch.txt"
            branch_file.write_text("feat-iter1")
            state.artifacts.bind("pr-branch", Uri.parse("file://session/pr-branch.txt"))
            _set_marker(state)
        return Done()

    loop = LoopStage(
        "test",
        body_runners=[runner],
        max_iterations=2,
        on_iteration_start=detach_to_pr_base,
    )
    asyncio.run(loop.run(state))

    assert detach_calls == ["feat-iter1"]
