"""Tests for gremlins.stages.exec.Exec."""

from __future__ import annotations

import asyncio
import pathlib

import pytest

from gremlins.artifacts.registry import MissingArtifact
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.stages.exec import Exec
from gremlins.stages.outcome import Bail, Done, NeedsFix


def _make_state(tmp_path: pathlib.Path, **kw):
    kw.setdefault("worktree", tmp_path)
    session_dir = tmp_path / "artifacts"
    session_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        session_dir=session_dir,
        **kw,
    )


def _exec(name: str = "test", cmds=None, *, in_map=None, out_map=None, on_fail=None):
    options = {}
    if cmds is not None:
        options["cmds"] = cmds
    if on_fail:
        options["on_fail"] = on_fail
    return Exec(name, options, in_map=in_map, out_map=out_map)


# ---------------------------------------------------------------------------
# Happy path — no in/out
# ---------------------------------------------------------------------------


def test_no_in_out_returns_done(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"])
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)


def test_no_cmds_returns_done(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=[])
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)


# ---------------------------------------------------------------------------
# in: artifact injection
# ---------------------------------------------------------------------------


def test_in_map_injects_env_var(tmp_path):
    state = _make_state(tmp_path)
    (state.session_dir / "value.txt").write_text("hello")
    state.artifacts.bind("my-key", Uri.parse("file://session/value.txt"))

    out_file = tmp_path / "captured.txt"
    stage = _exec(
        cmds=[f'echo "$MY_VAR" > {out_file}'],
        in_map={"MY_VAR": "my-key"},
    )
    asyncio.run(stage.run(state))
    assert out_file.read_text().strip() == "hello"


def test_in_map_missing_artifact_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], in_map={"X": "not-bound"})
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(state))


# ---------------------------------------------------------------------------
# out: file://session/<name>
# ---------------------------------------------------------------------------


def test_out_file_scheme_binds_and_verifies(tmp_path):
    state = _make_state(tmp_path)
    (state.session_dir / "out.txt").write_text("data")
    stage = _exec(cmds=["true"], out_map={"result": "file://session/out.txt"})
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    assert state.artifacts.produced("result")
    assert state.artifacts.resolve("result") == Uri.parse("file://session/out.txt")


def test_out_file_scheme_missing_file_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], out_map={"result": "file://session/missing.txt"})
    with pytest.raises(FileNotFoundError):
        asyncio.run(stage.run(state))


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
    result = asyncio.run(stage.run(state))
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
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    assert state.artifacts.produced("commits")


# ---------------------------------------------------------------------------
# out: {read:KEY} URI substitution
# ---------------------------------------------------------------------------


def test_out_read_sub_resolves_uri(tmp_path, monkeypatch):
    state = _make_state(tmp_path)
    monkeypatch.setattr(
        "gremlins.artifacts.schemes.GitHubResolver.verify_produced",
        lambda self, uri: None,
    )
    foo_file = state.session_dir / "foo.txt"
    stage = _exec(
        cmds=[f'echo 42 > "{foo_file}"'],
        out_map={
            "foo": "file://session/foo.txt",
            "bar": "gh://pr/{read:foo}",
        },
    )
    result = asyncio.run(stage.run(state))
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
        asyncio.run(stage.run(state))


def test_out_read_sub_unbound_key_raises(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["true"], out_map={"bar": "gh://pr/{read:nonexistent}"})
    with pytest.raises(MissingArtifact):
        asyncio.run(stage.run(state))


# ---------------------------------------------------------------------------
# Non-zero exit
# ---------------------------------------------------------------------------


def test_nonzero_exit_raises_bail(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["exit 1"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))


def test_nonzero_exit_writes_log(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec("myname", cmds=["echo oops; exit 1"])
    with pytest.raises(Bail):
        asyncio.run(stage.run(state))
    assert (state.session_dir / "exec-myname.log").exists()


def test_on_fail_needs_fix_returns_needs_fix(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(cmds=["exit 2"], on_fail="needs_fix")
    result = asyncio.run(stage.run(state))
    assert isinstance(result, NeedsFix)
    assert result.returncode == 2


def test_on_fail_needs_fix_binds_log_output(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec(
        "cmd",
        cmds=["echo boom; exit 1"],
        on_fail="needs_fix",
        out_map={"verify_log": "file://session/exec-{name}.log"},
    )
    result = asyncio.run(stage.run(state))
    assert isinstance(result, NeedsFix)
    assert state.artifacts.produced("verify_log")
    assert state.artifacts.resolve("verify_log") == Uri.parse(
        "file://session/exec-cmd.log"
    )


def test_success_writes_log(tmp_path):
    state = _make_state(tmp_path)
    stage = _exec("myname", cmds=["echo hello"])
    result = asyncio.run(stage.run(state))
    assert isinstance(result, Done)
    assert (state.session_dir / "exec-myname.log").exists()
