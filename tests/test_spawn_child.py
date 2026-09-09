"""Tests for gremlins.spawn.child."""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
from collections.abc import Generator
from typing import Any

import pytest
from _gremlins_core.clients import CLIENT_FACTORIES
from _gremlins_core.schemas import STAGE_TYPES
from _gremlins_core.stages import Bail, Done, Outcome

import gremlins.spawn.child as _rc
from gremlins.stages.base import Stage
from tests.fake_client import FakeClient


class _SimpleStage(Stage):
    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Stage:
        return cls(d["name"])


class _DoneStage(_SimpleStage):
    type = "_test_done"

    async def run(self, gremlin) -> Outcome:  # type: ignore[no-untyped-def]
        return Done()


class _BailStage(_SimpleStage):
    type = "_test_bail"

    async def run(self, gremlin) -> Outcome:  # type: ignore[no-untyped-def]
        raise Bail("security concern")


class _RaiseStage(_SimpleStage):
    type = "_test_raise"

    async def run(self, gremlin) -> Outcome:  # type: ignore[no-untyped-def]
        raise RuntimeError("something went wrong")


@pytest.fixture(autouse=True)
def _register_test_stages(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    monkeypatch.setitem(STAGE_TYPES, "_test_done", _DoneStage)
    monkeypatch.setitem(STAGE_TYPES, "_test_bail", _BailStage)
    monkeypatch.setitem(STAGE_TYPES, "_test_raise", _RaiseStage)

    saved = dict(CLIENT_FACTORIES)
    CLIENT_FACTORIES["fake"] = lambda _model, _extra=None: FakeClient(fixtures={})
    yield
    CLIENT_FACTORIES.clear()
    CLIENT_FACTORIES.update(saved)


def _write_spec(
    tmp_path: pathlib.Path,
    stage_type: str,
    *,
    extra: dict[str, Any] | None = None,
) -> pathlib.Path:
    spec: dict[str, Any] = {
        "stage_dict": {"name": "test-stage", "type": stage_type},
        "client": "fake:fake",
        "artifact_dir": str(tmp_path / "artifacts"),
    }
    if extra:
        spec.update(extra)
    spec_path = tmp_path / "spec.json"
    spec_path.write_text(json.dumps(spec), encoding="utf-8")
    return spec_path


def _read_result(spec_path: pathlib.Path) -> dict[str, Any]:
    result_path = pathlib.Path(str(spec_path) + ".result")
    assert result_path.exists(), f"result file not written: {result_path}"
    return dict(json.loads(result_path.read_text(encoding="utf-8")))


def test_load_spec_missing_file(tmp_path: pathlib.Path) -> None:
    with pytest.raises(FileNotFoundError):
        _rc._load_spec(tmp_path / "nonexistent.json")


def test_load_spec_not_a_dict(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "spec.json"
    p.write_text("[1, 2, 3]", encoding="utf-8")
    with pytest.raises(ValueError, match="JSON object"):
        _rc._load_spec(p)


def test_load_spec_invalid_json(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "spec.json"
    p.write_text("{not valid json", encoding="utf-8")
    with pytest.raises(Exception):
        _rc._load_spec(p)


def test_build_state_missing_client() -> None:
    from gremlins.executor.gremlin import Gremlin

    with pytest.raises(ValueError, match="client"):
        Gremlin.from_subprocess({"artifact_dir": "/tmp/x"})


def test_build_state_missing_artifact_dir() -> None:
    from gremlins.executor.gremlin import Gremlin

    with pytest.raises(ValueError, match="artifact_dir"):
        Gremlin.from_subprocess({"client": "fake:fake"})


def test_run_done(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_done")
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 0
    result = _read_result(spec_path)
    assert result["status"] == "done"
    assert result["detail"] == ""
    assert result["returncode"] is None


def test_run_bail(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_bail")
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 1
    result = _read_result(spec_path)
    assert result["status"] == "bail"
    assert result["detail"] == "security concern"


def test_run_stage_raises(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_raise")
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 2
    result = _read_result(spec_path)
    assert result["status"] == "error"
    assert "something went wrong" in result["detail"]


def test_run_bad_spec_missing_stage_dict(tmp_path: pathlib.Path) -> None:
    spec_path = tmp_path / "spec.json"
    spec_path.write_text(
        json.dumps({"client": "fake:fake", "artifact_dir": str(tmp_path)}),
        encoding="utf-8",
    )
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 2
    result = _read_result(spec_path)
    assert result["status"] == "error"
    assert "stage_dict" in result["detail"]


def test_main_no_args() -> None:
    assert _rc.main([]) == 1


def test_main_too_many_args() -> None:
    assert _rc.main(["a", "b"]) == 1


def test_main_missing_spec(tmp_path: pathlib.Path) -> None:
    rc = _rc.main([str(tmp_path / "missing.json")])
    assert rc == 1


def test_run_bail_records_stage_progress(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """state.json records stage name before run, even if the stage bails."""
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))

    gremlin_id = "test-bail-progress"
    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "state.json").write_text(
        json.dumps({"id": gremlin_id, "attempt": "a1", "status": "running"}),
        encoding="utf-8",
    )

    spec_path = _write_spec(
        tmp_path,
        "_test_bail",
        extra={"gremlin_id": gremlin_id},
    )
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 1
    result = _read_result(spec_path)
    assert result["status"] == "bail"

    state_json: dict[str, Any] = json.loads(
        (state_dir / "state.json").read_text(encoding="utf-8")
    )
    assert state_json.get("stage") == "test-stage", (
        f"stage should be recorded before bail, got {state_json.get('stage')!r}"
    )


def test_run_bail_preserves_child_stage_in_own_state(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Parallel child writes child name as top-level stage to its own state.json."""
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))

    child_id = "child-abc"
    child_dir = tmp_path / "state" / child_id
    child_dir.mkdir(parents=True, exist_ok=True)
    (child_dir / "state.json").write_text(
        json.dumps({"id": child_id, "attempt": "a1", "status": "running"}),
        encoding="utf-8",
    )

    # Simulate a parallel child spec via the spawn/child schema (child_id + parent_stage)
    spec_path = _write_spec(
        tmp_path,
        "_test_bail",
        extra={
            "gremlin_id": child_id,
            "child_id": child_id,
            "parent_stage": "parallel-group",
        },
    )
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 1

    state_json: dict[str, Any] = json.loads(
        (child_dir / "state.json").read_text(encoding="utf-8")
    )
    # Child writes to its own state.json, so the top-level `stage` is the child stage name.
    # `parent_stage` is NOT forwarded — the child's resume target is its single stage.
    assert state_json.get("stage") == "test-stage", (
        f"child stage should be recorded as top-level stage, got {state_json.get('stage')!r}"
    )
    # sub_stage must not leak into the child's own state.json.
    assert "sub_stage" not in state_json, (
        f"child state should not have sub_stage, got {state_json.get('sub_stage')!r}"
    )


def test_run_patches_child_pid(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """_run() writes os.getpid() to the child's state.json."""
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))

    child_id = "child-pid-test"
    child_dir = tmp_path / "state" / child_id
    child_dir.mkdir(parents=True, exist_ok=True)
    (child_dir / "state.json").write_text(
        json.dumps({"id": child_id, "attempt": "a1", "status": "running"}),
        encoding="utf-8",
    )

    spec_path = _write_spec(
        tmp_path,
        "_test_done",
        extra={"gremlin_id": child_id, "child_id": child_id},
    )
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 0

    state_json: dict[str, Any] = json.loads(
        (child_dir / "state.json").read_text(encoding="utf-8")
    )
    assert state_json.get("pid") == os.getpid(), (
        f"child state should record PID, got {state_json.get('pid')!r}"
    )


def test_main_happy_path(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_done")
    rc = _rc.main([str(spec_path)])
    assert rc == 0
    result = _read_result(spec_path)
    assert result["status"] == "done"
