"""Tests for gremlins.stages.exec.Exec."""

from __future__ import annotations

import asyncio
import pathlib

import pytest
from conftest import _TestGremlin

from gremlins.artifacts.registry import MissingArtifact
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.stages.exec import Exec
from gremlins.stages.outcome import Bail, Done


def _make_state(tmp_path: pathlib.Path, **kw):
    kw.setdefault("worktree", tmp_path)
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        artifact_dir=artifact_dir,
        **kw,
    )


def _exec(name: str = "test", cmds=None, *, in_map=None, out_map=None, timeout=None):
    options = {}
    if cmds is not None:
        options["cmds"] = cmds
    if timeout is not None:
        options["timeout"] = timeout
    return Exec(name, options, in_map=in_map, out_map=out_map)


# ---------------------------------------------------------------------------
# Happy path — no in/out
# ---------------------------------------------------------------------------


def test_no_in_out_returns_done(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"])
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)


def test_no_cmds_returns_done(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=[])
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)


# ---------------------------------------------------------------------------
# in: artifact injection
# ---------------------------------------------------------------------------


def test_in_map_injects_env_var(tmp_path):
    state = _make_state(tmp_path)
    (state.artifact_dir / "value.txt").write_text("hello")
    state.artifacts.bind("my-key", Uri.parse("file://session/value.txt"))

    out_file = tmp_path / "captured.txt"
    stage = _exec(
        cmds=[f'echo "$MY_VAR" > {out_file}'],
        in_map={"MY_VAR": "my-key"},
    )
    asyncio.run(stage.run(_TestGremlin(state)))
    assert out_file.read_text().strip() == "hello"


def test_in_map_missing_artifact_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], in_map={"X": "not-bound"})
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(_TestGremlin(state)))


# ---------------------------------------------------------------------------
# out: file://session/<name>
# ---------------------------------------------------------------------------


def test_out_file_scheme_binds_and_verifies(tmp_path):
    state = _make_state(tmp_path)
    (state.artifact_dir / "out.txt").write_text("data")
    stage = _exec(cmds=["true"], out_map={"result": "file://session/out.txt"})
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)
    assert state.artifacts.produced("result")
    assert state.artifacts.resolve("result") == Uri.parse("file://session/out.txt")


def test_out_file_scheme_missing_file_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], out_map={"result": "file://session/missing.txt"})
    with pytest.raises(FileNotFoundError):
        asyncio.run(stage.run(_TestGremlin(state)))


# ---------------------------------------------------------------------------
# out: git://range
# ---------------------------------------------------------------------------


def test_out_git_range_binds_commit_range(tmp_path, monkeypatch):
    state = _make_state(tmp_path)
    monkeypatch.setattr(
        "gremlins.stages.exec.snapshot_head_before", lambda cwd=None: "abc123"
    )
    monkeypatch.setattr(
        "gremlins.artifacts.registry.git_utils.head_sha", lambda cwd=None: "def456"
    )
    stage = _exec(cmds=["true"], out_map={"commits": "git://range"})
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)
    assert state.artifacts.produced("commits")
    bound_uri = state.artifacts.resolve("commits")
    assert str(bound_uri) == "git://range/abc123..def456"


def test_out_git_range_empty_diff_still_binds(tmp_path, monkeypatch):
    state = _make_state(tmp_path)
    monkeypatch.setattr(
        "gremlins.stages.exec.snapshot_head_before", lambda cwd=None: "abc123"
    )
    # HEAD doesn't advance — same SHA before and after
    monkeypatch.setattr(
        "gremlins.artifacts.registry.git_utils.head_sha", lambda cwd=None: "abc123"
    )
    stage = _exec(cmds=["true"], out_map={"commits": "git://range"})
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)
    assert state.artifacts.produced("commits")


# ---------------------------------------------------------------------------
# out: {read:KEY} URI substitution
# ---------------------------------------------------------------------------


def test_out_read_sub_resolves_uri(tmp_path):
    state = _make_state(tmp_path)
    foo_file = state.artifact_dir / "foo.txt"
    stage = _exec(
        cmds=[f'echo 42 > "{foo_file}"'],
        out_map={
            "foo": "file://session/foo.txt",
            "bar": "gh://pr/{read:foo}",
        },
    )
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)
    assert state.artifacts.resolve("bar") == Uri.parse("gh://pr/42")


def test_out_read_sub_backward_ref_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(
        cmds=["true"],
        out_map={
            "bar": "gh://pr/{read:foo}",
            "foo": "file://session/foo.txt",
        },
    )
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(_TestGremlin(state)))


def test_out_read_sub_unbound_key_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], out_map={"bar": "gh://pr/{read:nonexistent}"})
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(_TestGremlin(state)))


# ---------------------------------------------------------------------------
# Non-zero exit
# ---------------------------------------------------------------------------


def test_nonzero_exit_raises_bail(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["exit 1"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(_TestGremlin(state)))


def test_nonzero_exit_writes_log(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec("myname", cmds=["echo oops; exit 1"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(_TestGremlin(state)))
    assert (state.artifact_dir / "exec-myname.log").exists()


def test_success_writes_log(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec("myname", cmds=["echo hello"])
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)
    assert (state.artifact_dir / "exec-myname.log").exists()


# ---------------------------------------------------------------------------
# timeout option
# ---------------------------------------------------------------------------


def test_timeout_with_status_out_yields_needs_fix(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(
        cmds=["sleep 10"],
        out_map={"status": "file://session/status"},
        timeout=0.05,
    )
    result = asyncio.run(stage.run(_TestGremlin(state)))
    assert isinstance(result, Done)
    assert state.artifacts.read("status") == "needs_fix"


def test_timeout_without_status_out_raises_bail(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["sleep 10"], timeout=0.05)
    with pytest.raises(Bail):
        asyncio.run(stage.run(_TestGremlin(state)))
