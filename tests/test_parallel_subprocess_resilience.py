"""Tests for subprocess supervisor resilience: kill, crash, timeout, large output."""

from __future__ import annotations

import asyncio
import dataclasses
import functools
import json
import os
import pathlib
import signal
from collections.abc import Callable
from typing import Any

import pytest
from conftest import make_parent_state

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state, write_state
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.stages.parallel import ParallelStage
from gremlins.utils import proc as _proc_mod

# ---------------------------------------------------------------------------
# Fake subprocess helpers
# ---------------------------------------------------------------------------


class _FakeStreamReader:
    def __init__(self, data: bytes = b"") -> None:
        self._buf = data

    async def read(self, n: int) -> bytes:
        chunk, self._buf = self._buf[:n], self._buf[n:]
        return chunk


class _FakeProcess:
    """Simulates an asyncio subprocess with controllable behavior."""

    def __init__(
        self,
        exit_code: int = 0,
        *,
        hang: bool = False,
        stderr_data: bytes = b"",
    ) -> None:
        self._exit_code = exit_code
        self._hang = hang
        self.returncode: int | None = None if hang else exit_code
        self.stdout = _FakeStreamReader()
        self.stderr = _FakeStreamReader(stderr_data)
        self._event: asyncio.Event | None = None
        self.pid = id(self) & 0x7FFFFFFF  # unique per instance; os.killpg is patched
        _FAKE_PROCS[self.pid] = self

    def _ev(self) -> asyncio.Event:
        if self._event is None:
            self._event = asyncio.Event()
            if not self._hang:
                self._event.set()
        return self._event

    async def wait(self) -> int:
        await self._ev().wait()
        return self._exit_code

    def send_signal(self, sig: int) -> None:
        self.returncode = -abs(sig)
        self._ev().set()

    def kill(self) -> None:
        self.send_signal(signal.SIGKILL)


# ---------------------------------------------------------------------------
# Test helpers
# ---------------------------------------------------------------------------


_FAKE_PROCS: dict[int, _FakeProcess] = {}


@pytest.fixture(autouse=True)
def _patch_killpg(monkeypatch: pytest.MonkeyPatch) -> Any:
    """Route os.killpg in proc module to the fake process registered by pid.

    The real proc.py uses os.killpg() (POSIX, works on macOS and Linux). Tests
    here use _FakeProcess instead of real subprocesses, so we intercept killpg
    and forward to the fake's send_signal.
    """

    def _fake_killpg(pgid: int, sig: int) -> None:
        proc = _FAKE_PROCS.get(pgid)
        if proc is None:
            raise ProcessLookupError(pgid)
        proc.send_signal(sig)

    monkeypatch.setattr(os, "killpg", _fake_killpg)
    yield
    _FAKE_PROCS.clear()


def _child_stage(name: str) -> Stage:
    """Minimal stage with raw_dict set so _dispatch takes the subprocess path."""

    class _Noop(Stage):
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
        client=FakeClaudeClient(),
        artifact_dir=artifact_dir,
    )


def _run_parallel(
    stages: list[Stage],
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
        project_root=project_root,
        child_stages=stages,
    )
    rt_by_name = {name: fn for name, fn in rt}
    return rt_by_name["g"]  # the parallel-executor stage (group_name)


def _write_result(spec_path: pathlib.Path, status: str = "done") -> None:
    result_path = pathlib.Path(str(spec_path) + ".result")
    result_path.write_text(
        json.dumps(
            {"status": status, "detail": "", "returncode": None, "cost_usd": 0.0}
        ),
        encoding="utf-8",
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_external_kill_records_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Child killed by signal (negative returncode, no result file) → RuntimeError naming the child."""
    # asyncio reports signal-terminated children with a negative returncode.
    fake_proc = _FakeProcess(exit_code=-signal.SIGKILL)

    async def _mock_exec(*_args: Any, **_kwargs: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    with pytest.raises(RuntimeError, match=r"child-a.*SIGKILL.*no result file"):
        asyncio.run(parallel())  # type: ignore[operator]


def test_external_kill_siblings_continue(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When one child is killed, its sibling still runs to completion."""
    ran: list[str] = []

    async def _mock_exec(*args: Any, **_kwargs: Any) -> _FakeProcess:
        # Determine which child this call is for by spec path argument.
        spec_path = pathlib.Path(args[-1])
        child_key = spec_path.parent.name
        if child_key == "child-a":
            return _FakeProcess(exit_code=-signal.SIGKILL)  # killed, no result
        # child-b: exit 0, write result
        _write_result(spec_path)
        ran.append("child-b")
        return _FakeProcess(exit_code=0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    stage_a = _child_stage("child-a")
    stage_b = _child_stage("child-b")
    state_a = _child_state(tmp_path / "child-a")
    state_b = _child_state(tmp_path / "child-b")
    parent_data = StateData()
    parallel = _run_parallel(
        [stage_a, stage_b], [state_a, state_b], make_parent_state(parent_data), tmp_path
    )

    with pytest.raises(RuntimeError, match="child-a"):
        asyncio.run(parallel())  # type: ignore[operator]

    assert "child-b" in ran


def test_crash_before_result_records_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Child exits 0 but never writes result file → RuntimeError with clear reason."""
    fake_proc = _FakeProcess(exit_code=0)

    async def _mock_exec(*_args: Any, **_kwargs: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    with pytest.raises(RuntimeError, match="exited 0 without writing result"):
        asyncio.run(parallel())  # type: ignore[operator]


def test_timeout_kills_child_and_records_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Stage with timeout_seconds: child hangs → killed, RuntimeError mentioning timeout."""
    fake_proc = _FakeProcess(hang=True)

    async def _mock_exec(*_args: Any, **_kwargs: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    stage = _child_stage("child-a")
    stage.raw_dict = {
        "name": "child-a",
        "type": "_resilience_noop",
        "timeout_seconds": 0.05,
    }
    state = _child_state(tmp_path / "child-a")
    parallel = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    with pytest.raises(RuntimeError, match="timed out"):
        asyncio.run(parallel())  # type: ignore[operator]

    assert fake_proc.returncode is not None  # process was killed


def test_large_stderr_drains_without_deadlock(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Child emitting 1 MB of stderr completes without deadlock."""
    large_output = b"x" * (1024 * 1024)

    async def _mock_exec(*args: Any, **_kwargs: Any) -> _FakeProcess:
        spec_path = pathlib.Path(args[-1])
        _write_result(spec_path, status="done")
        return _FakeProcess(exit_code=0, stderr_data=large_output)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    asyncio.run(parallel())  # type: ignore[operator]  # must not hang or raise


def test_cancellation_sigterm_then_sigkill(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Cancellation sends SIGTERM; if child hangs past grace, SIGKILL is sent."""
    fake_proc = _FakeProcess(hang=True)
    # Make SIGTERM also not stop the process to force SIGKILL path.
    sigtermed = False

    orig_send = fake_proc.send_signal

    def _slow_send(sig: int) -> None:
        nonlocal sigtermed
        if sig == signal.SIGTERM:
            sigtermed = True
            # Don't unblock wait() — simulate process ignoring SIGTERM.
            return
        orig_send(sig)

    fake_proc.send_signal = _slow_send  # type: ignore[method-assign]

    async def _mock_exec(*_args: Any, **_kwargs: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)
    # Make the grace period very short so the test doesn't actually wait 10s.
    monkeypatch.setattr(
        _proc_mod,
        "terminate_with_grace",
        functools.partial(_proc_mod.terminate_with_grace, grace_s=0.05),
    )

    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    async def _run_and_cancel() -> None:
        task = asyncio.create_task(parallel())  # type: ignore[arg-type]
        await asyncio.sleep(0.01)
        task.cancel()
        try:
            await task
        except (asyncio.CancelledError, RuntimeError):
            pass

    asyncio.run(_run_and_cancel())

    assert sigtermed
    assert fake_proc.returncode is not None  # SIGKILL was sent


def test_subprocess_result_done_bail_error(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    p = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    async def mock_done(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "done")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", mock_done)
    asyncio.run(p())

    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    p = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    async def mock_bail(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "bail")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", mock_bail)
    asyncio.run(p())

    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    p = _run_parallel([stage], [state], make_parent_state(StateData()), tmp_path)

    async def mock_err(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "error")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", mock_err)
    with pytest.raises(RuntimeError, match="error"):
        asyncio.run(p())


def test_subprocess_cost_accumulated_in_state(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """cost_usd from each subprocess result is folded into state.json subprocess_cost_usd."""
    state_dir = tmp_path / "state" / "test-gremlin"
    state_dir.mkdir(parents=True)
    write_state(state_dir, {"id": "test-gremlin"})
    sf = state_dir / "state.json"

    parent_data = dataclasses.replace(
        StateData(gremlin_id="test-gremlin"), state_file=sf
    )
    parent_state = build_state(
        data=parent_data,
        client=FakeClaudeClient(),
        artifact_dir=state_dir,
    )

    stage_a = _child_stage("child-a")
    stage_b = _child_stage("child-b")
    session_a = tmp_path / "child-a"
    session_b = tmp_path / "child-b"
    session_a.mkdir(parents=True)
    session_b.mkdir(parents=True)

    def _make_child_state(session: pathlib.Path) -> State:
        return build_state(
            data=dataclasses.replace(
                StateData(gremlin_id="test-gremlin"), state_file=sf
            ),
            client=FakeClaudeClient(),
            artifact_dir=session,
        )

    runners: list[tuple[str, State, Callable[[], Any]]] = [
        (stage_a.name, _make_child_state(session_a), lambda: None),
        (stage_b.name, _make_child_state(session_b), lambda: None),
    ]
    rt = ParallelStage("g", [stage_a, stage_b]).build_runtime_stages(
        runners,
        parent_state=parent_state,
        project_root=tmp_path,
        child_stages=[stage_a, stage_b],
    )
    parallel_fn = dict(rt)["g"]

    COST_A, COST_B = 0.30, 0.12

    async def _mock_exec(*args: Any, **_kwargs: Any) -> _FakeProcess:
        spec_path = pathlib.Path(args[-1])
        child_key = spec_path.parent.name
        cost = COST_A if child_key == "child-a" else COST_B
        result_path = pathlib.Path(str(spec_path) + ".result")
        result_path.write_text(
            json.dumps(
                {"status": "done", "detail": "", "returncode": None, "cost_usd": cost}
            ),
            encoding="utf-8",
        )
        return _FakeProcess(exit_code=0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)
    asyncio.run(parallel_fn())

    data = json.loads(sf.read_text())
    assert data.get("subprocess_cost_usd") == pytest.approx(COST_A + COST_B)
