"""Tests for gremlins/fleet/session_summary.py."""

from __future__ import annotations

import io
import json
import os
import pathlib
import subprocess
import time

import gremlins.fleet.constants as _const
import gremlins.fleet.session_summary as ss
from gremlins.fleet.session_summary import (
    _collect_gremlins,
    _prune_old_state,
    _render_summary,
    main,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_state(
    state_root: pathlib.Path,
    gr_id: str,
    project_root: str,
    *,
    status: str = "running",
    kind: str = "localgremlin",
    stage: str = "plan",
    pid: int | None = None,
    exit_code: int | None = None,
    description: str = "",
    finished: bool = False,
    summarized: bool = False,
    closed: bool = False,
    log_text: str = "log line\n",
) -> pathlib.Path:
    wdir = state_root / gr_id
    wdir.mkdir(parents=True, exist_ok=True)
    state = {
        "id": gr_id,
        "kind": kind,
        "status": status,
        "stage": stage,
        "project_root": project_root,
    }
    if pid is not None:
        state["pid"] = pid
    if exit_code is not None:
        state["exit_code"] = exit_code
    if description:
        state["description"] = description
    (wdir / "state.json").write_text(json.dumps(state), encoding="utf-8")
    (wdir / "log").write_text(log_text)
    if finished:
        (wdir / "finished").touch()
    if summarized:
        (wdir / "summarized").touch()
    if closed:
        (wdir / "closed").touch()
    return wdir


def _invoke(
    hook_event: str,
    project_root: str,
    state_root: pathlib.Path,
    monkeypatch,
    capsys,
    *,
    skip_summary: bool = False,
) -> tuple[int, str, str]:
    """Call main() with a faked stdin, returning (rc, stdout, stderr)."""
    hook_input = json.dumps({"hook_event_name": hook_event, "cwd": project_root})

    monkeypatch.setenv("XDG_STATE_HOME", str(state_root.parent))
    monkeypatch.delenv("CLAUDE_PROJECT_DIR", raising=False)
    if skip_summary:
        monkeypatch.setenv("GREMLIN_SKIP_SUMMARY", "1")
    else:
        monkeypatch.delenv("GREMLIN_SKIP_SUMMARY", raising=False)

    # Patch git so project_root resolves to itself (no git call needed in tests).
    original_run = subprocess.run

    def fake_run(cmd, **kwargs):
        if cmd[:3] == ["git", "-C", project_root]:

            class R:
                stdout = project_root + "\n"
                returncode = 0

            return R()
        return original_run(cmd, **kwargs)

    monkeypatch.setattr(subprocess, "run", fake_run)

    monkeypatch.setattr("sys.stdin", io.StringIO(hook_input))

    rc = main([])
    captured = capsys.readouterr()
    return rc, captured.out, captured.err


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_no_gremlins_empty_state_root(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    state_root.mkdir()
    rc, out, err = _invoke("SessionStart", "/proj", state_root, monkeypatch, capsys)
    assert rc == 0
    assert out == ""
    assert err == ""


def test_running_gremlin_shown_at_session_start(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(
        state_root, "gr-abc123", project_root, pid=os.getpid(), stage="implement"
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "running" in out or "running" in err
    assert "gr-abc123" in out
    assert "gr-abc123" in err


def test_running_gremlin_not_shown_at_user_prompt_submit(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(
        state_root, "gr-abc123", project_root, pid=os.getpid(), stage="implement"
    )

    rc, out, err = _invoke(
        "UserPromptSubmit", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert out == ""
    assert err == ""


def test_newly_finished_gremlin_shown_at_session_start(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    wdir = _make_state(
        state_root,
        "gr-done01",
        project_root,
        status="running",
        finished=True,
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "gr-done01" in out
    assert "gr-done01" in err
    assert "finished since last check" in out
    assert (wdir / "summarized").exists()


def test_newly_finished_gremlin_shown_at_user_prompt_submit(
    tmp_path, monkeypatch, capsys
):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    wdir = _make_state(
        state_root,
        "gr-done02",
        project_root,
        status="running",
        finished=True,
    )

    rc, out, err = _invoke(
        "UserPromptSubmit", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "gr-done02" in out
    assert (wdir / "summarized").exists()


def test_already_summarized_gremlin_not_re_announced(tmp_path, monkeypatch, capsys):
    # A gremlin that was already summarized must not appear in the
    # "finished since last check" block again. It may still appear in the
    # running block (as dead:finished) because the bash running-block filter
    # only checks status=="running", not the summarized marker.
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(
        state_root,
        "gr-old01",
        project_root,
        finished=True,
        summarized=True,
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "finished since last check" not in out


def test_closed_gremlin_not_in_finished_block(tmp_path, monkeypatch, capsys):
    # A gremlin with the closed marker must not appear in the finished block.
    # (The closed marker only gates the finished-block listing; the running-block
    # filter only checks status=="running" and doesn't consult closed.)
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    # Use status="dead" so the gremlin does not trigger the running-block filter
    # either, making it fully absent from the output.
    _make_state(
        state_root,
        "gr-closed1",
        project_root,
        status="dead",
        finished=True,
        closed=True,
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "gr-closed1" not in out
    assert "gr-closed1" not in err


def test_stalled_gremlin_shown_as_stalled(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    wdir = _make_state(
        state_root,
        "gr-stall1",
        project_root,
        pid=os.getpid(),
        stage="implement",
    )
    # Make the log file appear old to trigger stall heuristic.
    old_time = time.time() - 4000
    os.utime(str(wdir / "log"), (old_time, old_time))

    monkeypatch.setattr(_const, "BG_STALL_SECS", 100)

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "stalled?" in out
    assert "gr-stall1" in out


def test_crashed_gremlin_shown_as_crashed(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    # Use a PID that definitely doesn't exist.
    _make_state(
        state_root,
        "gr-crash1",
        project_root,
        pid=999999999,
        stage="plan",
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "crashed" in out or "dead" in out
    assert "gr-crash1" in out


def test_skip_summary_env_short_circuits(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(state_root, "gr-skip01", project_root, finished=True)

    rc, out, err = _invoke(
        "SessionStart",
        project_root,
        state_root,
        monkeypatch,
        capsys,
        skip_summary=True,
    )
    assert rc == 0
    assert out == ""
    assert err == ""


def test_project_root_mismatch_filtered_out(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    _make_state(state_root, "gr-other1", "/other-project", finished=True)

    rc, out, err = _invoke(
        "SessionStart", "/myproject", state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "gr-other1" not in out
    assert "gr-other1" not in err


def test_prune_removes_closed_state_dir_older_than_14_days(tmp_path):
    state_root = tmp_path / "claude-gremlins"
    state_root.mkdir()
    old_wdir = state_root / "gr-old-closed"
    old_wdir.mkdir()
    (old_wdir / "state.json").write_text("{}")
    closed = old_wdir / "closed"
    closed.touch()
    old_time = time.time() - 15 * 86400
    os.utime(str(closed), (old_time, old_time))

    _prune_old_state(str(state_root))

    assert not old_wdir.exists()


def test_prune_keeps_closed_state_dir_younger_than_14_days(tmp_path):
    state_root = tmp_path / "claude-gremlins"
    state_root.mkdir()
    new_wdir = state_root / "gr-new-closed"
    new_wdir.mkdir()
    (new_wdir / "state.json").write_text("{}")
    (new_wdir / "closed").touch()  # mtime is now → not pruned

    _prune_old_state(str(state_root))

    assert new_wdir.exists()


def test_prune_removes_old_direct_dirs(tmp_path):
    state_root = tmp_path / "claude-gremlins"
    state_root.mkdir()
    direct = state_root / "direct"
    direct.mkdir()
    old_dir = direct / "20240101-000000-abc123"
    old_dir.mkdir()
    old_time = time.time() - 15 * 86400
    os.utime(str(old_dir), (old_time, old_time))

    _prune_old_state(str(state_root))

    assert not old_dir.exists()


def test_stdout_is_valid_json_with_hook_event_name(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(state_root, "gr-json01", project_root, finished=True)

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    data = json.loads(out)
    assert "hookSpecificOutput" in data
    assert data["hookSpecificOutput"]["hookEventName"] == "SessionStart"
    assert "IMPORTANT:" in data["hookSpecificOutput"]["additionalContext"]


def test_exit_code_shown_in_finished_block(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(
        state_root,
        "gr-exit01",
        project_root,
        status="running",
        finished=True,
        exit_code=2,
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "exit 2" in out


def test_description_shown_in_summary(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "claude-gremlins"
    project_root = "/myproject"
    _make_state(
        state_root,
        "gr-desc01",
        project_root,
        finished=True,
        description="add feature X",
    )

    rc, out, err = _invoke(
        "SessionStart", project_root, state_root, monkeypatch, capsys
    )
    assert rc == 0
    assert "add feature X" in out


def test_always_exits_zero_on_exception(monkeypatch, capsys):
    # Force an exception inside _run by making os.path.isdir raise.
    def boom(*a, **kw):
        raise RuntimeError("simulated failure")

    monkeypatch.setattr(ss, "_get_state_root", boom)
    rc = main([])
    assert rc == 0


# ---------------------------------------------------------------------------
# Unit tests for _render_summary
# ---------------------------------------------------------------------------


def test_render_summary_running_only():
    running = [
        {
            "id": "gr-r1",
            "kind": "localgremlin",
            "live": "running",
            "stage": "plan",
            "pid": "123",
            "description": "",
            "log": "/path/log",
        }
    ]
    summary = _render_summary(running, [])
    assert "**Background gremlins — running:**" in summary
    assert "gr-r1" in summary
    assert "finished since last check" not in summary


def test_render_summary_finished_only():
    finished = [
        {
            "id": "gr-f1",
            "kind": "ghgremlin",
            "status": "running",
            "exit_code": "",
            "description": "",
            "log": "/path/log",
            "wdir": "/wdir",
        }
    ]
    summary = _render_summary([], finished)
    assert "**Background gremlins — finished since last check:**" in summary
    assert "gr-f1" in summary
    assert "**Background gremlins — running:**" not in summary


def test_render_summary_both_blocks_separated_by_blank_line():
    running = [
        {
            "id": "gr-r2",
            "kind": "localgremlin",
            "live": "running",
            "stage": "plan",
            "pid": "456",
            "description": "",
            "log": "/log",
        }
    ]
    finished = [
        {
            "id": "gr-f2",
            "kind": "localgremlin",
            "status": "running",
            "exit_code": "",
            "description": "",
            "log": "/log",
            "wdir": "/w",
        }
    ]
    summary = _render_summary(running, finished)
    # The two blocks should be separated by a blank line (\n\n).
    assert "\n\n" in summary
    assert summary.index("running:**") < summary.index("finished since")


def test_render_summary_empty_when_both_empty():
    assert _render_summary([], []) == ""


def test_collect_gremlins_skips_no_state_json(tmp_path):
    state_root = tmp_path / "state"
    state_root.mkdir()
    # Directory with no state.json
    (state_root / "orphan").mkdir()
    running, finished, dirs = _collect_gremlins(str(state_root), "/proj")
    assert running == []
    assert finished == []
    assert dirs == []
