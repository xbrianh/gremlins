"""Tests for gremlins.stages.exec.Exec."""

from __future__ import annotations

import asyncio
import pathlib

import pytest
from _gremlins_core.artifacts import MissingArtifact, Uri
from _gremlins_core.executor import StateData, build_state
from _gremlins_core.stages import Bail, Done, Exec
from conftest import MockGremlin

from tests.fake_client import FakeClient


def _make_state(tmp_path: pathlib.Path, **kw):
    kw.setdefault("worktree", tmp_path)
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=FakeClient(),
        artifact_dir=artifact_dir,
        **kw,
    )


def _exec(
    name: str = "test",
    cmds=None,
    *,
    interpolation_map=None,
    bind_map=None,
    timeout=None,
):
    options = {}
    if cmds is not None:
        options["cmds"] = cmds
    if timeout is not None:
        options["timeout"] = timeout
    return Exec(name, options, interpolation_map=interpolation_map, bind_map=bind_map)


# ---------------------------------------------------------------------------
# Happy path — no in/out
# ---------------------------------------------------------------------------


def test_no_in_out_returns_done(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"])
    result = asyncio.run(stage.run(MockGremlin(state=state)))
    assert isinstance(result, Done)


def test_no_cmds_returns_done(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=[])
    result = asyncio.run(stage.run(MockGremlin(state=state)))
    assert isinstance(result, Done)


# ---------------------------------------------------------------------------
# in: artifact injection
# ---------------------------------------------------------------------------


def test_interpolation_map_substitutes_in_cmds(tmp_path):
    state = _make_state(tmp_path)
    state.artifacts.write_into_registry(Uri.parse("artifact://value.txt"), "hello")

    out_file = tmp_path / "captured.txt"
    stage = _exec(
        cmds=[f'echo "{{MY_VAR}}" > {out_file}'],
        interpolation_map={"MY_VAR": 'content("artifact://value.txt")'},
    )
    asyncio.run(stage.run(MockGremlin(state=state)))
    assert out_file.read_text().strip() == "hello"


def test_interpolation_map_missing_artifact_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], interpolation_map={"X": "not-bound"})
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(MockGremlin(state=state)))


# ---------------------------------------------------------------------------
# out: file://session/<name>
# ---------------------------------------------------------------------------


def test_bind_file_scheme_binds_and_verifies(tmp_path):
    state = _make_state(tmp_path)
    (state.artifact_dir / "out.txt").write_text("data")
    stage = _exec(cmds=["true"], bind_map={"result": "file://session/out.txt"})
    result = asyncio.run(stage.run(MockGremlin(state=state)))
    assert isinstance(result, Done)
    assert state.artifacts.is_registered("file://session/out.txt")
    # the registry binds the resolved filesystem path, not the original URI


def test_bind_file_scheme_missing_file_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], bind_map={"result": "file://session/missing.txt"})
    with pytest.raises(Bail, match="was not produced"):
        asyncio.run(stage.run(MockGremlin(state=state)))


def test_bind_recovers_from_stale_registration(tmp_path):
    """A deleted output file must not block the stage from recreating it."""
    state = _make_state(tmp_path)
    stale = state.artifacts.write_into_registry(
        Uri.parse("file://session/out.txt"), "stale"
    )
    pathlib.Path(stale).unlink()
    stage = _exec(
        cmds=["echo fresh > {result}"], bind_map={"result": "file://session/out.txt"}
    )
    result = asyncio.run(stage.run(MockGremlin(state=state)))
    assert isinstance(result, Done)
    assert state.artifacts.content("file://session/out.txt", None).strip() == "fresh"


# ---------------------------------------------------------------------------
# loop_iter in bind URIs
# ---------------------------------------------------------------------------


def test_loop_iter_in_bind_uri(tmp_path):
    """{loop_iter} in bind URIs is resolved before registration."""
    state = _make_state(tmp_path)
    state.loop_stack = [("test-exec", 3)]
    (state.artifact_dir / "test-exec~3" / "out.txt").parent.mkdir(
        parents=True, exist_ok=True
    )
    (state.artifact_dir / "test-exec~3" / "out.txt").write_text("data")
    stage = _exec(
        cmds=["true"],
        bind_map={"result": "artifact://{loop_iter}/out.txt"},
    )
    result = asyncio.run(stage.run(MockGremlin(state=state)))
    assert isinstance(result, Done)
    assert state.artifacts.is_registered("artifact://test-exec~3/out.txt")


# ---------------------------------------------------------------------------
# Non-zero exit
# ---------------------------------------------------------------------------


def test_nonzero_exit_raises_bail(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["exit 1"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(MockGremlin(state=state)))


def test_nonzero_exit_writes_log(tmp_path):
    state = _make_state(tmp_path)
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    stage = _exec("myname", cmds=["echo oops; exit 1"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(MockGremlin(state=state, state_dir=state_dir)))
    assert (state_dir / "exec-myname.log").exists()


def test_success_writes_log(tmp_path):
    state = _make_state(tmp_path)
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    stage = _exec("myname", cmds=["echo hello"])
    result = asyncio.run(stage.run(MockGremlin(state=state, state_dir=state_dir)))
    assert isinstance(result, Done)
    assert (state_dir / "exec-myname.log").exists()


# ---------------------------------------------------------------------------
# timeout option
# ---------------------------------------------------------------------------


def test_timeout_raises_bail(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["sleep 10"], timeout=0.05)
    with pytest.raises(Bail):
        asyncio.run(stage.run(MockGremlin(state=state)))


# ---------------------------------------------------------------------------
# bail artifact
# ---------------------------------------------------------------------------


def test_bail_artifact_on_exit_2(tmp_path):
    """Exit code 2 with bail in bind_map writes the bail artifact."""
    state = _make_state(tmp_path)
    bail_file = state.artifact_dir / "bail"
    bail_file.write_text("something broke")
    stage = _exec(
        cmds=["exit 2"],
        bind_map={"bail": "artifact://bail"},
    )
    result = asyncio.run(stage.run(MockGremlin(state=state)))
    assert isinstance(result, Done)
    assert state.artifacts.is_registered("artifact://bail")
