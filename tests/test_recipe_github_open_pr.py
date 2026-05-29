"""Tests for gremlins:github-open-pr recipe behavior."""

from __future__ import annotations

import asyncio
import pathlib

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.stages.exec import Exec


def _make_state(tmp_path: pathlib.Path, loop_iteration: int = 1):
    session_dir = tmp_path / "artifacts"
    session_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(loop_iteration=loop_iteration),
        client=FakeClaudeClient(),
        session_dir=session_dir,
        worktree=tmp_path,
    )


_BRANCH_CMD = """\
BASE="$(cat "{session_dir}/pr-branch.txt")" &&
SUFFIX=$([ "{loop_iteration}" -gt 1 ] && echo "-iter{loop_iteration}" || echo "") &&
BRANCH="${BASE}${SUFFIX}" &&
printf "%s" "$BRANCH" > "{session_dir}/branch-out.txt"
"""


def _run_branch_cmd(tmp_path: pathlib.Path, loop_iteration: int, base: str) -> str:
    state = _make_state(tmp_path, loop_iteration=loop_iteration)
    (state.session_dir / "pr-branch.txt").write_text(base)
    stage = Exec("push-and-open", {"cmds": [_BRANCH_CMD]})
    asyncio.run(stage.run(state))
    return (state.session_dir / "branch-out.txt").read_text()


def test_branch_name_unchanged_on_first_iteration(tmp_path: pathlib.Path) -> None:
    assert _run_branch_cmd(tmp_path, loop_iteration=1, base="fix-thing") == "fix-thing"


def test_branch_name_appends_iter2_on_second_iteration(tmp_path: pathlib.Path) -> None:
    assert _run_branch_cmd(tmp_path, loop_iteration=2, base="fix-thing") == "fix-thing-iter2"


def test_branch_name_appends_iterN_on_later_iterations(tmp_path: pathlib.Path) -> None:
    assert _run_branch_cmd(tmp_path, loop_iteration=5, base="fix-thing") == "fix-thing-iter5"
