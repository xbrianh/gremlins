"""Tests for subprocess supervisor resilience: kill, crash, timeout, large output."""

from __future__ import annotations

import asyncio
import json
import pathlib
import signal
from collections.abc import Callable
from typing import Any

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.stages.parallel import ParallelStage

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


def _child_stage(name: str) -> Stage:
    """Minimal stage with raw_dict set so _dispatch takes the subprocess path."""

    class _Noop(Stage):
        type = "_resilience_noop"

        async def run(self, state: State) -> Outcome:  # noqa: ARG002
            return Done()

    s = _Noop(name)
    s.raw_dict = {"name": name, "type": "_resilience_noop"}
    return s


def _child_state(session_dir: pathlib.Path) -> State:
    session_dir.mkdir(parents=True, exist_ok=True)
    return State(
        data=StateData(),
        client=FakeClaudeClient(),
        session_dir=session_dir,
    )


def _run_parallel(
    stages: list[Stage],
    states: list[State],
    parent_data: StateData,
    project_root: pathlib.Path,
) -> Callable[[], Any]:
    runners: list[tuple[str, State, Callable[[], Any]]] = [
        (s.name, st, lambda: None) for s, st in zip(stages, states)
    ]
    rt = ParallelStage("g", stages).build_runtime_stages(
        runners,
        parent_data=parent_data,
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
    parallel = _run_parallel([stage], [state], StateData(), tmp_path)

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
        [stage_a, stage_b], [state_a, state_b], parent_data, tmp_path
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
    parallel = _run_parallel([stage], [state], StateData(), tmp_path)

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
    parallel = _run_parallel([stage], [state], StateData(), tmp_path)

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
    parallel = _run_parallel([stage], [state], StateData(), tmp_path)

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
    monkeypatch.setattr("gremlins.stages.parallel._SIGTERM_GRACE_S", 0.05)

    stage = _child_stage("child-a")
    state = _child_state(tmp_path / "child-a")
    parallel = _run_parallel([stage], [state], StateData(), tmp_path)

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
    p = _run_parallel([stage], [state], StateData(), tmp_path)

    async def mock_done(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "done")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", mock_done)
    asyncio.run(p())

    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    p = _run_parallel([stage], [state], StateData(), tmp_path)

    async def mock_bail(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "bail")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", mock_bail)
    asyncio.run(p())

    stage = _child_stage("c")
    state = _child_state(tmp_path / "c")
    p = _run_parallel([stage], [state], StateData(), tmp_path)

    async def mock_err(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "error")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", mock_err)
    with pytest.raises(RuntimeError, match="error"):
        asyncio.run(p())
