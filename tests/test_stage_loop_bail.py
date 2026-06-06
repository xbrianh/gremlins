"""Tests for the bail marker primitive in Exec + LoopStage."""

from __future__ import annotations

import asyncio
import pathlib
import subprocess
from typing import TYPE_CHECKING, cast

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.stages.exec import Exec
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Bail

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


def _make_state(tmp_path: pathlib.Path):
    artifact_dir = tmp_path / "session"
    artifact_dir.mkdir()
    subprocess.run(
        ["git", "init", "-b", "main"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "config", "user.email", "t@t.com"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "T"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    (tmp_path / "f.txt").write_text("x")
    subprocess.run(
        ["git", "add", "f.txt"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "init"], cwd=tmp_path, check=True, capture_output=True
    )
    return build_state(
        data=StateData(gremlin_id=None),
        client=FakeClaudeClient(fixtures={}),
        artifact_dir=artifact_dir,
        worktree=tmp_path,
    )


def _make_gremlin_wrapper(state: State):  # type: ignore[name-defined]
    """Wrap a State in a _Gremlin object for passing to Stage.run()."""
    class _Gremlin:  # noqa: N801
        def __init__(self, state: State) -> None:
            self.state = state
            self.registry = state.artifacts

    return cast("Gremlin", _Gremlin(state))


def test_bail_in_body_exec_terminates_loop(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    exec_stage = Exec(
        "check",
        {"cmds": ["printf 'bail reason' > '{artifact_dir}/bail'", "exit 2"]},
        out_map={"bail": "file://session/bail"},
    )
    loop = LoopStage("loop", body=[exec_stage], max_iterations=3)

    with pytest.raises(Bail) as exc_info:
        asyncio.run(loop.run(_make_gremlin_wrapper(state)))

    assert "bail reason" in exc_info.value.reason


def test_bail_message_matches_file_contents(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    exec_stage = Exec(
        "check",
        {
            "cmds": [
                "printf 'specific: assertion broken' > '{artifact_dir}/bail'",
                "exit 2",
            ]
        },
        out_map={"bail": "file://session/bail"},
    )
    loop = LoopStage("loop", body=[exec_stage], max_iterations=3)

    with pytest.raises(Bail) as exc_info:
        asyncio.run(loop.run(_make_gremlin_wrapper(state)))

    assert exc_info.value.reason == "specific: assertion broken"


def test_bail_wins_over_needs_fix(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    exec_stage = Exec(
        "check",
        {"cmds": ["printf 'hard stop' > '{artifact_dir}/bail'", "exit 2"]},
        out_map={
            "bail": "file://session/bail",
            "status": "file://session/status",
        },
    )
    loop = LoopStage("loop", body=[exec_stage], max_iterations=3)

    with pytest.raises(Bail) as exc_info:
        asyncio.run(loop.run(_make_gremlin_wrapper(state)))

    assert exc_info.value.reason == "hard stop"
