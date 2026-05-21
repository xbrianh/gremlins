"""Focused tests for proc.run_child_subprocess and its helpers."""

from __future__ import annotations

import asyncio
import json
import pathlib
import signal
from typing import Any

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils import proc

# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class _FakeStreamReader:
    def __init__(self, data: bytes = b"") -> None:
        self._buf = data

    async def read(self, n: int) -> bytes:
        chunk, self._buf = self._buf[:n], self._buf[n:]
        return chunk


class _FakeProcess:
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


def _stage(name: str, timeout: float | None = None) -> Stage:
    class _Noop(Stage):
        type = "_proc_test_noop"

        async def run(self, state: State) -> Outcome:
            return Done()

    s = _Noop(name)
    d: dict[str, Any] = {"name": name, "type": "_proc_test_noop"}
    if timeout is not None:
        d["timeout_seconds"] = timeout
    s.raw_dict = d
    return s


def _state(session_dir: pathlib.Path) -> State:
    session_dir.mkdir(parents=True, exist_ok=True)
    return State(
        data=StateData(),
        client=FakeClaudeClient(),
        session_dir=session_dir,
    )


def _write_result(spec_path: pathlib.Path, status: str, cost: float = 0.0) -> None:
    result_path = pathlib.Path(str(spec_path) + ".result")
    result_path.write_text(
        json.dumps(
            {"status": status, "detail": "d", "returncode": None, "cost_usd": cost}
        ),
        encoding="utf-8",
    )


# ---------------------------------------------------------------------------
# parse_child_timeout
# ---------------------------------------------------------------------------


def test_parse_child_timeout_none_when_no_raw_dict() -> None:
    s = _stage("x")
    s.raw_dict = None
    assert proc._parse_child_timeout(s, "x") is None


def test_parse_child_timeout_none_when_missing_key() -> None:
    assert proc._parse_child_timeout(_stage("x"), "x") is None


def test_parse_child_timeout_returns_value() -> None:
    s = _stage("x", timeout=30.0)
    assert proc._parse_child_timeout(s, "x") == 30.0


def test_parse_child_timeout_zero_treated_as_none() -> None:
    s = _stage("x")
    s.raw_dict = {"timeout_seconds": 0}
    assert proc._parse_child_timeout(s, "x") is None


def test_parse_child_timeout_invalid_raises() -> None:
    s = _stage("x")
    s.raw_dict = {"timeout_seconds": "bad"}
    with pytest.raises(ValueError, match="must be a number"):
        proc._parse_child_timeout(s, "x")


# ---------------------------------------------------------------------------
# missing_result_detail
# ---------------------------------------------------------------------------


def test_missing_result_detail_exit_zero() -> None:
    msg = proc._missing_result_detail("child-a", 0)
    assert "exited 0 without writing result" in msg


def test_missing_result_detail_signal() -> None:
    msg = proc._missing_result_detail("child-a", -signal.SIGKILL)
    assert "SIGKILL" in msg
    assert "no result file" in msg


def test_missing_result_detail_nonzero() -> None:
    msg = proc._missing_result_detail("child-a", 42)
    assert "returncode 42" in msg


def test_missing_result_detail_no_returncode() -> None:
    msg = proc._missing_result_detail("child-a", None)
    assert "unavailable" in msg


# ---------------------------------------------------------------------------
# run_child_subprocess — happy path
# ---------------------------------------------------------------------------


def test_run_child_done_status(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")
    bailed: list[str] = []

    async def _mock_exec(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "done", cost=0.5)
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    status, cost = asyncio.run(
        proc.run_child_subprocess(
            stage, child_st, "c", "attempt-1", on_bail=bailed.append
        )
    )
    assert status == "done"
    assert cost == pytest.approx(0.5)
    assert bailed == []


def test_run_child_needs_fix_maps_to_done(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")

    async def _mock_exec(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "needs_fix")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    status, _ = asyncio.run(
        proc.run_child_subprocess(
            stage, child_st, "c", "attempt-1", on_bail=lambda _: None
        )
    )
    assert status == "done"


# ---------------------------------------------------------------------------
# run_child_subprocess — bail status
# ---------------------------------------------------------------------------


def test_run_child_bail_calls_on_bail(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")
    bailed: list[str] = []

    async def _mock_exec(*args: Any, **_: Any) -> _FakeProcess:
        result_path = pathlib.Path(str(args[-1]) + ".result")
        result_path.write_text(
            json.dumps({"status": "bail", "detail": "nope", "cost_usd": 0.0}),
            encoding="utf-8",
        )
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    status, _ = asyncio.run(
        proc.run_child_subprocess(
            stage, child_st, "c", "attempt-1", on_bail=bailed.append
        )
    )
    assert status == "bail"
    assert bailed == ["nope"]


# ---------------------------------------------------------------------------
# run_child_subprocess — error status
# ---------------------------------------------------------------------------


def test_run_child_error_status_raises(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")

    async def _mock_exec(*args: Any, **_: Any) -> _FakeProcess:
        _write_result(pathlib.Path(args[-1]), "error")
        return _FakeProcess(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    with pytest.raises(RuntimeError, match="error"):
        asyncio.run(
            proc.run_child_subprocess(
                stage, child_st, "c", "attempt-1", on_bail=lambda _: None
            )
        )


# ---------------------------------------------------------------------------
# run_child_subprocess — missing result file
# ---------------------------------------------------------------------------


def test_missing_result_file_raises(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")
    fake_proc = _FakeProcess(exit_code=-signal.SIGKILL)

    async def _mock_exec(*_: Any, **__: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    with pytest.raises(RuntimeError, match="SIGKILL.*no result file"):
        asyncio.run(
            proc.run_child_subprocess(
                stage, child_st, "c", "attempt-1", on_bail=lambda _: None
            )
        )


def test_exit_zero_no_result_file_raises(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")

    async def _mock_exec(*_: Any, **__: Any) -> _FakeProcess:
        return _FakeProcess(exit_code=0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    with pytest.raises(RuntimeError, match="exited 0 without writing result"):
        asyncio.run(
            proc.run_child_subprocess(
                stage, child_st, "c", "attempt-1", on_bail=lambda _: None
            )
        )


# ---------------------------------------------------------------------------
# run_child_subprocess — timeout
# ---------------------------------------------------------------------------


def test_timeout_raises_and_kills_child(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c", timeout=0.05)
    fake_proc = _FakeProcess(hang=True)

    async def _mock_exec(*_: Any, **__: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    with pytest.raises(RuntimeError, match="timed out"):
        asyncio.run(
            proc.run_child_subprocess(
                stage, child_st, "c", "attempt-1", on_bail=lambda _: None
            )
        )

    assert fake_proc.returncode is not None


# ---------------------------------------------------------------------------
# run_child_subprocess — signal-terminated child
# ---------------------------------------------------------------------------


def test_signal_terminated_child_no_result_raises(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    child_st = _state(tmp_path / "c")
    stage = _stage("c")
    fake_proc = _FakeProcess(exit_code=-signal.SIGTERM)

    async def _mock_exec(*_: Any, **__: Any) -> _FakeProcess:
        return fake_proc

    monkeypatch.setattr(asyncio, "create_subprocess_exec", _mock_exec)

    with pytest.raises(RuntimeError, match="SIGTERM.*no result file"):
        asyncio.run(
            proc.run_child_subprocess(
                stage, child_st, "c", "attempt-1", on_bail=lambda _: None
            )
        )
