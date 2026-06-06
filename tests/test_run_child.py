"""Tests for gremlins.run_child."""

from __future__ import annotations

import asyncio
import json
import pathlib
from collections.abc import Generator
from typing import Any

import pytest

import gremlins.run_child as _rc
from gremlins.clients.fake import FakeClaudeClient
from gremlins.clients.registry import CLIENT_FACTORIES, register_client_factory
from gremlins.executor.state import State
from gremlins.pipeline.loader import STAGE_TYPES
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Bail, Done, Outcome

# ---------------------------------------------------------------------------
# Test stage stubs
# ---------------------------------------------------------------------------


class _SimpleStage(Stage):
    """Base for test stages: with_dict constructs from name only."""

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


class _ArtifactStage(_SimpleStage):
    type = "_test_artifact"

    async def run(self, gremlin) -> Outcome:  # type: ignore[no-untyped-def]
        gremlin.state.data.append_artifact({"type": "branch", "name": "feat/test"})  # type: ignore[union-attr]
        return Done()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _register_test_stages(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    monkeypatch.setitem(STAGE_TYPES, "_test_done", _DoneStage)
    monkeypatch.setitem(STAGE_TYPES, "_test_bail", _BailStage)
    monkeypatch.setitem(STAGE_TYPES, "_test_raise", _RaiseStage)
    monkeypatch.setitem(STAGE_TYPES, "_test_artifact", _ArtifactStage)

    saved = dict(CLIENT_FACTORIES)
    register_client_factory(
        "fake", lambda _model, _policy: FakeClaudeClient(fixtures={})
    )
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


# ---------------------------------------------------------------------------
# _load_spec
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# _build_state validation
# ---------------------------------------------------------------------------


def test_build_state_missing_client() -> None:
    with pytest.raises(ValueError, match="client"):
        _rc._build_state({"artifact_dir": "/tmp/x"})


def test_build_state_missing_artifact_dir() -> None:
    with pytest.raises(ValueError, match="artifact_dir"):
        _rc._build_state({"client": "fake:fake"})


# ---------------------------------------------------------------------------
# _run: happy path
# ---------------------------------------------------------------------------


def test_run_done(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_done")
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 0
    result = _read_result(spec_path)
    assert result["status"] == "done"
    assert result["detail"] == ""
    assert result["returncode"] is None


# ---------------------------------------------------------------------------
# _run: Bail
# ---------------------------------------------------------------------------


def test_run_bail(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_bail")
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 1
    result = _read_result(spec_path)
    assert result["status"] == "bail"
    assert result["detail"] == "security concern"


# ---------------------------------------------------------------------------
# _run: unexpected exception
# ---------------------------------------------------------------------------


def test_run_stage_raises(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_raise")
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 2
    result = _read_result(spec_path)
    assert result["status"] == "error"
    assert "something went wrong" in result["detail"]


# ---------------------------------------------------------------------------
# _run: stage that produces artifacts (with a real state file)
# ---------------------------------------------------------------------------


def test_run_artifact_stage(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import gremlins.paths as _paths

    state_root = tmp_path / "state"
    monkeypatch.setattr(_paths, "state_root", lambda: state_root)

    gremlin_id = "test-gremlin-abc"
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "state.json").write_text(
        json.dumps({"id": gremlin_id, "attempt": "a1"}), encoding="utf-8"
    )

    spec_path = _write_spec(
        tmp_path,
        "_test_artifact",
        extra={"gremlin_id": gremlin_id},
    )
    rc = asyncio.run(_rc._run(spec_path))
    assert rc == 0
    result = _read_result(spec_path)
    assert result["status"] == "done"

    state_json: dict[str, Any] = json.loads(
        (state_dir / "state.json").read_text(encoding="utf-8")
    )
    artifacts: list[dict[str, Any]] = list(state_json.get("artifacts") or [])
    assert any(a.get("name") == "feat/test" for a in artifacts)


# ---------------------------------------------------------------------------
# _run: bad spec (missing stage_dict)
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# main(): argument handling and end-to-end
# ---------------------------------------------------------------------------


def test_main_no_args() -> None:
    assert _rc.main([]) == 1


def test_main_too_many_args() -> None:
    assert _rc.main(["a", "b"]) == 1


def test_main_missing_spec(tmp_path: pathlib.Path) -> None:
    rc = _rc.main([str(tmp_path / "missing.json")])
    assert rc == 1


def test_main_happy_path(tmp_path: pathlib.Path) -> None:
    spec_path = _write_spec(tmp_path, "_test_done")
    rc = _rc.main([str(spec_path)])
    assert rc == 0
    result = _read_result(spec_path)
    assert result["status"] == "done"
