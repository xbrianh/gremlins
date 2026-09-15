"""Tests for subprocess supervisor resilience: kill, crash, timeout, large output.

The parallel stage spawns each child with ``sys.executable -m gremlins.spawn.child
<spec>``. These tests point ``sys.executable`` at a fake interpreter that reads a
per-child plan from the environment, so the real spawn/pump/wait/teardown path is
exercised end to end without running a full gremlin.
"""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import signal
import stat
import sys
from collections.abc import Callable
from typing import Any

import _gremlins_core.stages as _parallel_mod
import pytest
from _gremlins_core.config import scratch_root
from _gremlins_core.executor import State, StateData, build_state, write_state
from _gremlins_core.stages import Done, Outcome, ParallelStage, StageAttrs
from conftest import make_parent_state

from gremlins.utils.proc import terminate_with_grace
from tests.fake_client import FakeClient

# A stand-in for ``python -m gremlins.spawn.child``. It ignores the ``-m`` module
# argument and treats the last argv entry as the spec path, then behaves per the
# ``FAKE_CHILD_PLAN`` entry for its child key.
_FAKE_CHILD = '''#!__PYTHON__
"""Fake ``gremlins.spawn.child`` driven by ``FAKE_CHILD_PLAN``.

Each plan entry is a step dict:
    {"mode": "done"|"sleep"|"hang"|"kill"|"exit_no_result"|"stderr_blob",
     "status": "done"|"bail"|"error"|"needs_fix", "detail": str,
     "cost": float, "exit": int, "bytes": int, "stdout": str}
"""
import json
import os
import pathlib
import signal
import sys
import time

spec = pathlib.Path(sys.argv[-1])
plan = json.loads(os.environ.get("FAKE_CHILD_PLAN", "{}"))
child_key = json.loads(spec.read_text()).get("child_key") or spec.parent.name
step = plan.get(child_key, {})

pid_file = os.environ.get("FAKE_CHILD_PID_FILE")
if pid_file:
    pathlib.Path(pid_file).write_text(str(os.getpid()))

mode = step.get("mode", "done")
if mode == "hang":
    # Ignore SIGTERM so teardown must escalate to SIGKILL; record that we saw it.
    marker = os.environ.get("FAKE_CHILD_SIGTERM_MARKER")
    if marker:

        def _on_term(*_args):
            pathlib.Path(marker).write_text("term")

        signal.signal(signal.SIGTERM, _on_term)
    else:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(60)
elif mode == "sleep":
    time.sleep(60)
elif mode == "kill":
    os.kill(os.getpid(), signal.SIGKILL)
elif mode == "exit_no_result":
    sys.exit(step.get("exit", 0))
elif mode == "stderr_blob":
    sys.stderr.write("x" * step.get("bytes", 1024 * 1024))
    sys.stderr.flush()

if step.get("stdout"):
    sys.stdout.write(step["stdout"])
    sys.stdout.flush()

pathlib.Path(str(spec) + ".result").write_text(
    json.dumps(
        {
            "status": step.get("status", "done"),
            "detail": step.get("detail", ""),
            "returncode": None,
            "cost_usd": step.get("cost", 0.0),
        }
    )
)
sys.exit(step.get("exit", 0))
'''


@pytest.fixture
def fake_child(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
    """Install the fake interpreter as ``sys.executable`` for the test."""
    script = tmp_path / "fake_child.py"
    script.write_text(_FAKE_CHILD.replace("__PYTHON__", sys.executable))
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setattr(sys, "executable", str(script))
    return script


@pytest.fixture
def child_plan(monkeypatch: pytest.MonkeyPatch) -> Callable[[dict[str, Any]], None]:
    """Set the per-child behaviour plan the fake interpreter reads."""

    def _set(plan: dict[str, Any]) -> None:
        monkeypatch.setenv("FAKE_CHILD_PLAN", json.dumps(plan))

    return _set


def _child_stage(name: str) -> StageAttrs:
    """Minimal stage with raw_dict set so dispatch takes the subprocess path."""

    class _Noop(StageAttrs):
        type = "_resilience_noop"

        async def run(self, gremlin) -> Outcome:  # type: ignore[override]
            return Done()

    s = _Noop(name)
    s.raw_dict = {"name": name, "type": "_resilience_noop"}
    return s


def _child_state(artifact_dir: pathlib.Path) -> State:
    artifact_dir.mkdir(parents=True, exist_ok=True)
    return build_state(
        data=StateData(),
        client=FakeClient(),
        artifact_dir=artifact_dir,
    )


def _run_parallel(
    stages: list[StageAttrs],
    states: list[State],
    parent_state: State,
    project_root: pathlib.Path,
) -> Callable[[], Any]:
    runners: list[tuple[str, State, Callable[[], Any]]] = [
        (s.name, st, lambda: None) for s, st in zip(stages, states)
    ]
    rt = ParallelStage("g", stages).build_runtime_stages(
        runners,
        parent_state=parent_state,
        project_root_path=project_root,
        child_stages=stages,
    )
    rt_by_name = {name: fn for name, fn in rt}
    return rt_by_name["g"]  # the parallel-executor stage (group_name)


def _run_one(
    tmp_path: pathlib.Path,
    stage: StageAttrs,
    state: State,
    parent_state: State | None = None,
) -> Callable[[], Any]:
    return _run_parallel(
        [stage], [state], parent_state or make_parent_state(StateData()), tmp_path
    )


def test_external_kill_records_failure(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """Child killed by signal (negative returncode, no result file) → RuntimeError naming the child."""
    child_plan({"child-a": {"mode": "kill"}})
    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_one(tmp_path, stage, state)

    with pytest.raises(RuntimeError, match=r"child-a.*SIGKILL.*no result file"):
        asyncio.run(parallel())  # type: ignore[operator]


def test_external_kill_siblings_continue(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """When one child is killed, its sibling still runs to completion."""
    child_plan({"child-a": {"mode": "kill"}, "child-b": {"mode": "done"}})
    stage_a = _child_stage("child-a")
    stage_b = _child_stage("child-b")
    state_a = _child_state(tmp_path / "child-a")
    state_b = _child_state(tmp_path / "child-b")
    parallel = _run_parallel(
        [stage_a, stage_b],
        [state_a, state_b],
        make_parent_state(StateData()),
        tmp_path,
    )

    with pytest.raises(RuntimeError, match="child-a"):
        asyncio.run(parallel())  # type: ignore[operator]

    # child-b wrote its result before the group surfaced child-a's failure.
    assert list((tmp_path / "child-b").glob("spec_*.json.result"))


def test_crash_before_result_records_failure(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """Child exits 0 but never writes result file → RuntimeError with clear reason."""
    child_plan({"child-a": {"mode": "exit_no_result", "exit": 0}})
    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_one(tmp_path, stage, state)

    with pytest.raises(RuntimeError, match="exited 0 without writing result"):
        asyncio.run(parallel())  # type: ignore[operator]


def test_timeout_kills_child_and_records_failure(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """Stage with timeout_seconds: child hangs → killed, RuntimeError mentioning timeout."""
    child_plan({"child-a": {"mode": "sleep"}})
    stage = _child_stage("child-a")
    stage.raw_dict = {
        "name": "child-a",
        "type": "_resilience_noop",
        "timeout_seconds": 0.05,
    }
    state = _child_state(tmp_path / "child-a")
    parallel = _run_one(tmp_path, stage, state)

    with pytest.raises(RuntimeError, match="timed out"):
        asyncio.run(parallel())  # type: ignore[operator]


def test_large_stderr_drains_without_deadlock(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """Child emitting 1 MB of stderr completes without deadlock."""
    child_plan({"child-a": {"mode": "stderr_blob", "bytes": 1024 * 1024}})
    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_one(tmp_path, stage, state)

    asyncio.run(parallel())  # type: ignore[operator]  # must not hang or raise


def _process_alive(pid: int) -> bool:
    """Whether `pid` still names a process (a zombie has not been reaped yet).

    ``os.kill(pid, 0)`` delivers no signal; it only probes existence, so it
    works on every POSIX platform rather than relying on Linux's ``/proc``.
    """
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # The process exists but belongs to another user.
        return True
    return True


def test_cancellation_terminates_child(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Cancelling the parallel future tears the child down via ChildGuard::drop."""
    child_plan({"child-a": {"mode": "sleep"}})
    pid_file = tmp_path / "child.pid"
    monkeypatch.setenv("FAKE_CHILD_PID_FILE", str(pid_file))

    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_one(tmp_path, stage, state)

    async def _run_and_cancel() -> None:
        task = asyncio.create_task(parallel())  # type: ignore[arg-type]
        # Wait for the child to start before cancelling.
        for _ in range(200):
            if pid_file.exists():
                break
            await asyncio.sleep(0.01)
        assert pid_file.exists(), "child never started"
        pid = int(pid_file.read_text())
        task.cancel()
        try:
            await task
        except (asyncio.CancelledError, RuntimeError):
            pass
        # ChildGuard hands the blocking teardown to a dedicated thread; give it
        # a moment to run before asserting the child is gone.
        for _ in range(500):
            if not _process_alive(pid):
                return
            await asyncio.sleep(0.01)
        raise AssertionError(f"child {pid} survived cancellation")

    asyncio.run(_run_and_cancel())


def test_subprocess_result_done_bail_error(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")

    child_plan({"c": {"status": "done"}})
    asyncio.run(_run_one(tmp_path, stage, state)())

    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    child_plan({"c": {"status": "bail"}})
    asyncio.run(_run_one(tmp_path, stage, state)())

    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    child_plan({"c": {"status": "error", "detail": "boom"}})
    with pytest.raises(RuntimeError, match="error"):
        asyncio.run(_run_one(tmp_path, stage, state)())


def test_subprocess_cost_accumulated_in_state(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """cost_usd from each subprocess result is folded into state.json subprocess_cost_usd."""
    state_dir = tmp_path / "state" / "test-gremlin"
    state_dir.mkdir(parents=True)
    write_state(state_dir, {"id": "test-gremlin"})
    sf = state_dir / "state.json"

    parent_data = StateData(gremlin_id="test-gremlin")
    parent_data.state_file = sf
    parent_state = build_state(
        data=parent_data,
        client=FakeClient(),
        artifact_dir=state_dir,
    )

    stage_a = _child_stage("child-a")
    stage_b = _child_stage("child-b")
    session_a = tmp_path / "child-a"
    session_b = tmp_path / "child-b"
    session_a.mkdir(parents=True)
    session_b.mkdir(parents=True)

    def _make_child_state(session: pathlib.Path) -> State:
        child_data = StateData(gremlin_id="test-gremlin")
        child_data.state_file = sf
        return build_state(
            data=child_data,
            client=FakeClient(),
            artifact_dir=session,
        )

    runners: list[tuple[str, State, Callable[[], Any]]] = [
        (stage_a.name, _make_child_state(session_a), lambda: None),
        (stage_b.name, _make_child_state(session_b), lambda: None),
    ]
    rt = ParallelStage("g", [stage_a, stage_b]).build_runtime_stages(
        runners,
        parent_state=parent_state,
        project_root_path=tmp_path,
        child_stages=[stage_a, stage_b],
    )
    parallel_fn = dict(rt)["g"]

    COST_A, COST_B = 0.30, 0.12
    child_plan(
        {
            "child-a": {"cost": COST_A},
            "child-b": {"cost": COST_B},
        }
    )
    asyncio.run(parallel_fn())

    data = json.loads(sf.read_text())
    assert data.get("subprocess_cost_usd") == pytest.approx(COST_A + COST_B)


def test_parse_child_timeout_none_when_no_raw_dict() -> None:
    s = _child_stage("x")
    s.raw_dict = None
    assert _parallel_mod._parse_child_timeout(s, "x") is None


def test_parse_child_timeout_none_when_missing_key() -> None:
    assert _parallel_mod._parse_child_timeout(_child_stage("x"), "x") is None


def test_parse_child_timeout_returns_value() -> None:
    s = _child_stage("x")
    s.raw_dict = {"name": "x", "type": "_resilience_noop", "timeout_seconds": 30.0}
    assert _parallel_mod._parse_child_timeout(s, "x") == 30.0


def test_parse_child_timeout_zero_treated_as_none() -> None:
    s = _child_stage("x")
    s.raw_dict = {"timeout_seconds": 0}
    assert _parallel_mod._parse_child_timeout(s, "x") is None


def test_parse_child_timeout_invalid_raises() -> None:
    s = _child_stage("x")
    s.raw_dict = {"timeout_seconds": "bad"}
    with pytest.raises(ValueError, match="must be a number"):
        _parallel_mod._parse_child_timeout(s, "x")


def test_missing_result_detail_exit_zero() -> None:
    msg = _parallel_mod._missing_result_detail("child-a", 0)
    assert "exited 0 without writing result" in msg


def test_missing_result_detail_signal() -> None:
    msg = _parallel_mod._missing_result_detail("child-a", -signal.SIGKILL)
    assert "SIGKILL" in msg
    assert "no result file" in msg


def test_missing_result_detail_nonzero() -> None:
    msg = _parallel_mod._missing_result_detail("child-a", 42)
    assert "returncode 42" in msg


def test_missing_result_detail_no_returncode() -> None:
    msg = _parallel_mod._missing_result_detail("child-a", None)
    assert "unavailable" in msg


def test_run_child_needs_fix_maps_to_done(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    child_plan({"c": {"status": "needs_fix"}})
    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    parallel = _run_one(tmp_path, stage, state)
    asyncio.run(parallel())  # needs_fix is treated as done; must not raise


def test_run_child_bail_is_recorded_not_raised(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    child_plan({"c": {"status": "bail", "detail": "nope"}})
    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    parallel = _run_one(tmp_path, stage, state)
    asyncio.run(parallel())  # a bail is recorded, not raised


def test_bailed_subprocess_child_not_marked_done(
    tmp_path: pathlib.Path,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """A bailed subprocess child must not be recorded as completed."""
    state_dir = tmp_path / "state" / "gr-bail-done"
    state_dir.mkdir(parents=True)
    write_state(state_dir, {"id": "gr-bail-done"})
    sf = state_dir / "state.json"

    parent_data = StateData(gremlin_id="gr-bail-done")
    parent_data.state_file = sf
    parent_state = build_state(
        data=parent_data, client=FakeClient(), artifact_dir=state_dir
    )

    child_plan({"c": {"status": "bail", "detail": "nope"}})
    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    parallel = _run_one(tmp_path, stage, state, parent_state)
    asyncio.run(parallel())

    done = json.loads(sf.read_text()).get("done_children", {}).get("g", [])
    assert "c" not in done


def test_build_child_spec_dict_base_ref_propagated(
    tmp_path: pathlib.Path,
) -> None:
    artifact_dir = tmp_path / "c"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    child_st = build_state(
        data=StateData(),
        client=FakeClient(),
        artifact_dir=artifact_dir,
        base_ref="main",
    )
    stage = _child_stage("c")
    spec = _parallel_mod._build_child_spec_dict(stage, child_st, "c", "attempt-1")
    assert spec["base_ref"] == "main"


def test_build_child_spec_dict_base_ref_empty_by_default(
    tmp_path: pathlib.Path,
) -> None:
    child_st = _child_state(tmp_path / "c")
    stage = _child_stage("c")
    spec = _parallel_mod._build_child_spec_dict(stage, child_st, "c", "attempt-1")
    assert spec["base_ref"] == ""


def test_child_logs_survive_fan_in_cleanup(
    sandbox: Any,
    fake_child: pathlib.Path,
    child_plan: Callable[[dict[str, Any]], None],
) -> None:
    """Child scratch logs are copied into the parent state logs/ dir before
    fan-in removes the child scratch directories."""
    gremlin_id = "test-log-save"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)
    write_state(state_dir, {"id": gremlin_id})
    sf = state_dir / "state.json"

    parent_data = StateData(gremlin_id=gremlin_id)
    parent_data.state_file = sf
    parent_state = build_state(
        data=parent_data, client=FakeClient(), artifact_dir=state_dir
    )

    stages = [_child_stage(k) for k in ("child-a", "child-b")]
    child_ids = ("child-a", "child-b")
    scratch_dirs = {
        key: pathlib.Path(scratch_root(f"{gremlin_id}--g--{key}")) for key in child_ids
    }
    states = []
    for key in child_ids:
        session = scratch_dirs[key] / "artifacts"
        session.mkdir(parents=True, exist_ok=True)
        child_data = StateData(gremlin_id=gremlin_id)
        child_data.state_file = sf
        states.append(
            build_state(data=child_data, client=FakeClient(), artifact_dir=session)
        )

    runners = [(s.name, st, lambda: None) for s, st in zip(stages, states)]
    rt = dict(
        ParallelStage("g", stages).build_runtime_stages(
            runners,
            parent_state=parent_state,
            project_root_path=sandbox.project,
            child_stages=stages,
        )
    )

    # Each child emits one stdout line; the pump relays it into the child's log.
    child_plan(
        {
            key: {"stdout": f"stream failed for {gremlin_id}--g--{key}\n"}
            for key in child_ids
        }
    )
    asyncio.run(rt["g"]())
    asyncio.run(rt["g-fanin"]())

    logs_dir = state_dir / "logs"
    for key in child_ids:
        assert (logs_dir / f"{key}.log").read_text(encoding="utf-8") == (
            f"stream failed for {gremlin_id}--g--{key}\n"
        )
        assert not scratch_dirs[key].exists() or not list(scratch_dirs[key].iterdir())


def test_child_scratch_cleaned_when_parent_state_dir_missing(sandbox: Any) -> None:
    """A missing parent state dir only skips log preservation; scratch still goes."""
    gremlin_id = "test-missing-state"
    child_scratch = pathlib.Path(scratch_root(f"{gremlin_id}--g--child-a"))
    (child_scratch / "artifacts").mkdir(parents=True, exist_ok=True)

    parent_data = StateData(gremlin_id=gremlin_id)
    parent_data.state_file = sandbox.state / gremlin_id / "state.json"
    parent_state = build_state(
        parent_data, FakeClient(), artifact_dir=sandbox.state / "artifacts"
    )
    _parallel_mod._remove_child_dirs(parent_state, ["child-a"], "g")
    assert not child_scratch.exists()


def test_terminate_with_grace_does_not_kill_descendants(
    tmp_path: pathlib.Path,
) -> None:
    """Real Rust terminate_with_grace kills only the target PID — descendants survive."""

    async def _run() -> None:
        pid_file = tmp_path / "grandchild_pid"
        pid_file.write_text("")

        proc = await asyncio.create_subprocess_exec(
            "sh",
            "-c",
            f"(sleep 999 & echo $! > {pid_file}) & sleep 999",
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
            start_new_session=True,
        )
        pid = proc.pid
        assert pid is not None

        # Wait for grandchild to start and write its PID.
        for _ in range(20):
            gc_pid = pid_file.read_text().strip()
            if gc_pid:
                break
            await asyncio.sleep(0.05)

        assert gc_pid, "grandchild did not start in time"

        try:
            # Call the real Rust binding (targets only the specific PID, not the PG).
            await terminate_with_grace(proc, grace_s=0.1)

            # Parent should be dead.
            ret = await proc.wait()
            assert ret != 0

            # Grandchild should still be alive (terminate_with_grace does not kill
            # the process group — it only targets the specific PID).
            gc_alive = await asyncio.create_subprocess_exec(
                "kill",
                "-0",
                gc_pid,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            ret = await gc_alive.wait()
            assert ret == 0, f"grandchild {gc_pid} died unexpectedly"
        finally:
            # Cleanup: kill the grandchild to avoid leaking a long-lived process.
            await asyncio.create_subprocess_exec(
                "kill",
                "-9",
                gc_pid,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )

    asyncio.run(_run())
