"""Tests for gremlins.stages.exec.Exec primitive."""

from __future__ import annotations

import asyncio
import pathlib
import subprocess

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData
from gremlins.stages.exec import Exec
from gremlins.stages.outcome import Bail, Done, NeedsFix


def _make_state(tmp_path: pathlib.Path) -> RuntimeState:
    return RuntimeState(
        data=StateData(),
        client=None,  # type: ignore[arg-type]
        session_dir=tmp_path,
        worktree=tmp_path,
        artifacts=ArtifactRegistry(tmp_path, cwd=tmp_path),
    )


def _init_git(path: pathlib.Path) -> None:
    for cmd in [
        ["git", "init"],
        ["git", "config", "user.email", "t@t"],
        ["git", "config", "user.name", "t"],
        ["git", "commit", "--allow-empty", "-m", "init"],
    ]:
        subprocess.run(cmd, cwd=path, check=True, capture_output=True)


def test_exec_runs_cmds_and_returns_done(tmp_path: pathlib.Path) -> None:
    stage = Exec("run", [], {"cmds": ["true"]})
    state = _make_state(tmp_path)
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)


def test_exec_no_cmds_returns_done(tmp_path: pathlib.Path) -> None:
    stage = Exec("empty", [], {})
    state = _make_state(tmp_path)
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)


def test_exec_bail_on_failure(tmp_path: pathlib.Path) -> None:
    stage = Exec("fail", [], {"cmds": ["false"]})
    state = _make_state(tmp_path)
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))


def test_exec_needs_fix_on_failure(tmp_path: pathlib.Path) -> None:
    stage = Exec("fail", [], {"cmds": ["false"], "on_fail": "needs_fix"})
    state = _make_state(tmp_path)
    result = asyncio.run(stage.run(state))
    assert isinstance(result, NeedsFix)
    assert result.returncode == 1


def test_exec_writes_log_on_failure(tmp_path: pathlib.Path) -> None:
    stage = Exec("myop", [], {"cmds": ["echo oops && false"], "on_fail": "needs_fix"})
    state = _make_state(tmp_path)
    asyncio.run(stage.run(state))
    log = tmp_path / "exec-myop.log"
    assert log.exists()
    assert "oops" in log.read_text()


def test_exec_in_map_sets_env(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    assert state.artifacts is not None
    state.artifacts.bind("mykey", Uri.parse("file://session/out.txt"))
    (tmp_path / "out.txt").write_text("hello-value")
    stage = Exec(
        "env-test",
        [],
        {"cmds": ['test "$MY_VAR" = hello-value']},
        in_map={"MY_VAR": "mykey"},
    )
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)


def test_exec_out_file_binds_artifact(tmp_path: pathlib.Path) -> None:
    (tmp_path / "out.txt").write_text("content")
    stage = Exec(
        "write-file",
        [],
        {"cmds": ["true"]},
        out_map={"report": "file://session/out.txt"},
    )
    state = _make_state(tmp_path)
    asyncio.run(stage.run(state))
    assert state.artifacts is not None
    assert state.artifacts.produced("report")


def test_exec_out_file_missing_raises(tmp_path: pathlib.Path) -> None:
    stage = Exec(
        "missing-file",
        [],
        {"cmds": ["true"]},
        out_map={"report": "file://session/nonexistent.txt"},
    )
    state = _make_state(tmp_path)
    with pytest.raises(FileNotFoundError):
        asyncio.run(stage.run(state))


def test_exec_out_git_range_binds_artifact(tmp_path: pathlib.Path) -> None:
    _init_git(tmp_path)
    stage = Exec(
        "commit",
        [],
        {
            "cmds": [
                "echo hi > file.txt",
                "git add file.txt",
                "git commit -m test",
            ]
        },
        out_map={"commits": "git://range"},
    )
    state = _make_state(tmp_path)
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    assert state.artifacts is not None
    assert state.artifacts.produced("commits")
    uri = state.artifacts.resolve("commits")
    assert uri.scheme == "git"
    assert "range/" in uri.path


def test_exec_with_dict_parses_in_out(tmp_path: pathlib.Path) -> None:
    d = {
        "name": "test-exec",
        "type": "exec",
        "in": {"MY_VAR": "somekey"},
        "out": {"result": "file://session/result.txt"},
        "options": {"cmds": ["true"]},
    }
    stage = Exec.with_dict(d)
    assert stage.in_map == {"MY_VAR": "somekey"}
    assert stage.out_map == {"result": "file://session/result.txt"}
