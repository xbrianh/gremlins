import asyncio
import pathlib
import subprocess

import pytest

from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.apply import Apply
from gremlins.stages.outcome import Bail, Done


def _apply_state(tmp_path: pathlib.Path) -> RuntimeState:
    return build_state(
        data=StateData(),
        client=None,
        session_dir=tmp_path,
        worktree=tmp_path,
    )


def _init_git_repo(path: pathlib.Path) -> None:
    subprocess.run(["git", "init"], cwd=path, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "t@t"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "t"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "--allow-empty", "-m", "init"],
        cwd=path,
        check=True,
        capture_output=True,
    )


def test_apply_success_with_changes(tmp_path):
    _init_git_repo(tmp_path)
    (tmp_path / "f.txt").write_text("x")
    stage = Apply("normalize", [], {"cmds": ["true"], "commit_message": "norm"})
    state = _apply_state(tmp_path)
    outcome = asyncio.run(stage.run(state))
    assert outcome == Done()
    log = subprocess.run(
        ["git", "log", "--oneline", "-1"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    )
    assert "norm" in log.stdout


def test_apply_binds_commit_range_on_success(tmp_path):
    _init_git_repo(tmp_path)
    (tmp_path / "f.txt").write_text("x")
    stage = Apply("apply-bind", [], {"cmds": ["true"], "commit_message": "bind"})
    state = _apply_state(tmp_path)
    asyncio.run(stage.run(state))
    assert state.artifacts.produced("apply-bind-commits")


def test_apply_no_commit_range_when_no_changes(tmp_path):
    _init_git_repo(tmp_path)
    stage = Apply("apply-nochange", [], {"cmds": ["true"]})
    state = _apply_state(tmp_path)
    asyncio.run(stage.run(state))
    assert not state.artifacts.produced("apply-nochange-commits")


def test_apply_reentry_does_not_rebind_commit_range(tmp_path):
    _init_git_repo(tmp_path)
    (tmp_path / "f.txt").write_text("x")
    stage = Apply("apply-reentry", [], {"cmds": ["true"], "commit_message": "re"})
    state = _apply_state(tmp_path)
    asyncio.run(stage.run(state))
    first_uri = str(state.artifacts.resolve("apply-reentry-commits"))
    # re-entry: no new changes, so no new commit, and the binding must not change
    asyncio.run(stage.run(state))
    assert str(state.artifacts.resolve("apply-reentry-commits")) == first_uri


def test_apply_success_no_changes(tmp_path):
    _init_git_repo(tmp_path)
    stage = Apply("n", [], {"cmds": ["true"]})
    state = _apply_state(tmp_path)
    outcome = asyncio.run(stage.run(state))
    assert outcome == Done()
    # no new commit: count should be 1
    cnt = subprocess.run(
        ["git", "rev-list", "--count", "HEAD"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    )
    assert cnt.stdout.strip() == "1"


def test_apply_cmd_failure_writes_log_and_raises(tmp_path):
    _init_git_repo(tmp_path)
    stage = Apply("n", [], {"cmds": ["false"]})
    state = _apply_state(tmp_path)
    with pytest.raises(Bail) as exc:
        asyncio.run(stage.run(state))
    assert "exited 1" in str(exc.value)
    assert (tmp_path / "apply.log").exists()


def test_apply_mid_cmd_failure_no_partial_commit(tmp_path):
    _init_git_repo(tmp_path)
    (tmp_path / "f.txt").write_text("x")
    stage = Apply("n", [], {"cmds": ["true", "false", "echo after"]})
    state = _apply_state(tmp_path)
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))
    assert (tmp_path / "apply.log").exists()
    # still only init commit
    cnt = subprocess.run(
        ["git", "rev-list", "--count", "HEAD"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    )
    assert cnt.stdout.strip() == "1"
