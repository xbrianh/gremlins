"""Tests for gremlins/fleet.py."""

import json
import os
import pathlib
import subprocess
from typing import TYPE_CHECKING

import pytest

import gremlins.cli.fleet as _fleet_cli

if TYPE_CHECKING:
    pass
import gremlins.fleet.ack as _ack
import gremlins.fleet.close as _close
import gremlins.fleet.constants as _constants
import gremlins.fleet.duration as _duration
import gremlins.fleet.land as _land
import gremlins.fleet.land as _land_mod
import gremlins.fleet.render as _render
import gremlins.fleet.state as _state
import gremlins.fleet.views as _views
from gremlins.cli.fleet import _main_impl

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _write_state(
    state_dir: pathlib.Path,
    payload: dict,
    *,
    finished: bool = False,
    log_text: str | None = None,
) -> str:
    state_dir.mkdir(parents=True, exist_ok=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps(payload))
    if finished:
        (state_dir / "finished").touch()
    if log_text is not None:
        (state_dir / "log").write_text(log_text)
    return str(sf)


def _setup_dead_gremlin(
    sandbox, tmp_path, gremlin_id="test-id-aabb12", **state_overrides
):
    """Build a state-root with a single dead gremlin."""
    state_root = sandbox.state
    gr_dir = state_root / gremlin_id
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "stage": "review-code",
        "status": "dead",
        "exit_code": 2,
        "workdir": str(workdir),
    }
    state.update(state_overrides)
    _write_state(gr_dir, state, finished=True)
    return gr_dir, workdir


def _init_git_repo(path: pathlib.Path) -> None:
    subprocess.run(
        ["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    (path / "README.md").write_text("init\n")
    subprocess.run(["git", "add", "."], cwd=path, check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True
    )


# ---------------------------------------------------------------------------
# liveness_of_state_file — state transitions
# ---------------------------------------------------------------------------


def test_liveness_running_with_live_pid_and_fresh_log(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": os.getpid()},
        log_text="recent",
    )
    assert _state.liveness_of_state_file(sf) == "running"


def test_liveness_finished_zero_exit(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": 99999, "exit_code": 0},
        finished=True,
    )
    assert _state.liveness_of_state_file(sf) == "finished"


def test_liveness_dead_with_nonzero_exit(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": 99999, "exit_code": 2},
        finished=True,
    )
    assert _state.liveness_of_state_file(sf) == "dead:exit 2"


def test_liveness_dead_bailed_includes_reason(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {"status": "bailed", "exit_code": 2, "bail_reason": "structural"},
        finished=True,
    )
    assert _state.liveness_of_state_file(sf) == "dead:bailed:structural"


def test_liveness_dead_crashed_when_pid_gone(tmp_path):
    # PID extremely unlikely to exist
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": 999999},
    )
    assert _state.liveness_of_state_file(sf).startswith("dead:crashed")


def test_liveness_stalled_when_log_is_old(tmp_path, monkeypatch):
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": os.getpid()},
        log_text="old",
    )
    log_path = tmp_path / "g" / "log"
    old = os.path.getmtime(log_path) - 10000
    os.utime(log_path, (old, old))
    monkeypatch.setattr(_constants, "BG_STALL_SECS", 100)
    assert _state.liveness_of_state_file(sf).startswith("stalled:")


def test_boss_waiting_with_old_log_shows_waiting_duration(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {
            "status": "running",
            "pid": os.getpid(),
            "kind": "bossgremlin",
            "stage": "waiting",
        },
        log_text="old",
    )
    log_path = tmp_path / "g" / "log"
    old = os.path.getmtime(log_path) - 200
    os.utime(log_path, (old, old))
    live = _state.liveness_of_state_file(sf)
    assert live.startswith("waiting (")
    assert "stalled" not in live


def test_boss_waiting_no_log_shows_waiting(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {
            "status": "running",
            "pid": os.getpid(),
            "kind": "bossgremlin",
            "stage": "waiting",
        },
    )
    live = _state.liveness_of_state_file(sf)
    assert live == "waiting"


def test_boss_non_waiting_stage_shows_running(tmp_path, monkeypatch):
    sf = _write_state(
        tmp_path / "g",
        {
            "status": "running",
            "pid": os.getpid(),
            "kind": "bossgremlin",
            "stage": "handoff",
        },
        log_text="old",
    )
    log_path = tmp_path / "g" / "log"
    old = os.path.getmtime(log_path) - 10000
    os.utime(log_path, (old, old))
    monkeypatch.setattr(_constants, "BG_STALL_SECS", 100)
    assert _state.liveness_of_state_file(sf) == "running"


def test_build_row_waiting_with_sub_stage():
    state = {
        "kind": "bossgremlin",
        "stage": "waiting",
        "sub_stage": "implement",
        "started_at": "",
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "waiting (3m12s)")
    assert row.stage == "waiting:implement"


def test_build_row_non_waiting_sub_stage_not_shown():
    state = {
        "kind": "localgremlin",
        "stage": "verify",
        "sub_stage": "cmd",
        "started_at": "",
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert row.stage == "verify"


def test_build_row_client_from_state():
    state = {
        "kind": "localgremlin",
        "stage": "implement",
        "started_at": "",
        "client": "copilot:gpt-5.4",
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert row.client == "copilot:gpt-5.4"


def test_build_row_client_missing_field_shows_dash():
    state = {"kind": "localgremlin", "stage": "implement", "started_at": ""}
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert row.client == "—"


def test_build_row_preserves_long_client_label():
    client = "copilot:gpt-5.4-super-long-client-label"
    state = {
        "kind": "localgremlin",
        "stage": "implement",
        "started_at": "",
        "client": client,
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert row.client == client


# ---------------------------------------------------------------------------
# do_close — close flow
# ---------------------------------------------------------------------------


def test_close_dead_gremlin_marks_closed(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(sandbox, tmp_path)
    ok = _close.do_close("test-id-aabb12")
    assert ok is True
    assert (gr_dir / "closed").exists()


def test_close_already_closed_is_idempotent(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(sandbox, tmp_path)
    (gr_dir / "closed").touch()
    ok = _close.do_close("test-id-aabb12")
    assert ok is True
    assert "already closed" in capsys.readouterr().out


def test_close_running_refused(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(sandbox, tmp_path)
    state = json.loads((gr_dir / "state.json").read_text())
    state["status"] = "running"
    state["pid"] = os.getpid()
    state["exit_code"] = None
    (gr_dir / "state.json").write_text(json.dumps(state))
    (gr_dir / "finished").unlink()
    (gr_dir / "log").write_text("recent")

    ok = _close.do_close("test-id-aabb12")
    assert ok is False
    assert not (gr_dir / "closed").exists()
    assert "still live" in capsys.readouterr().out


def test_close_not_found(sandbox, tmp_path, monkeypatch, capsys):
    ok = _close.do_close("nonexistent-id")
    assert ok is False
    assert "no gremlin matched" in capsys.readouterr().out


# ---------------------------------------------------------------------------
# do_ack / do_skip
# ---------------------------------------------------------------------------


def _setup_bailed_gremlin(
    sandbox, tmp_path, gremlin_id="test-id-aabb12", **state_overrides
):
    state_root = sandbox.state
    gr_dir = state_root / gremlin_id
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "ghgremlin",
        "stage": "github-review-pull-request",
        "status": "bailed",
        "bail_reason": "reviewer_requested_changes",
        "exit_code": 2,
        "workdir": str(workdir),
    }
    state.update(state_overrides)
    _write_state(gr_dir, state, finished=True)
    return gr_dir, workdir


def test_ack_on_bailed_child_sets_external_outcome_landed(
    sandbox, tmp_path, monkeypatch, capsys
):
    gr_dir, _ = _setup_bailed_gremlin(sandbox, tmp_path)
    ok = _ack.do_ack("test-id-aabb12")
    assert ok is True
    state = json.loads((gr_dir / "state.json").read_text())
    assert state["external_outcome"] == "landed"
    # other fields untouched
    assert state["status"] == "bailed"
    assert state["bail_reason"] == "reviewer_requested_changes"
    assert state["exit_code"] == 2


def test_ack_on_running_child_refuses(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        sandbox, tmp_path, status="running", exit_code=None
    )
    ok = _ack.do_ack("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_ack_on_completed_child_refuses(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        sandbox, tmp_path, status="completed", exit_code=0
    )
    ok = _ack.do_ack("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_skip_on_bailed_child_sets_external_outcome_abandoned(
    sandbox, tmp_path, monkeypatch, capsys
):
    gr_dir, _ = _setup_bailed_gremlin(sandbox, tmp_path)
    ok = _ack.do_skip("test-id-aabb12")
    assert ok is True
    state = json.loads((gr_dir / "state.json").read_text())
    assert state["external_outcome"] == "abandoned"
    # other fields untouched
    assert state["status"] == "bailed"
    assert state["bail_reason"] == "reviewer_requested_changes"
    assert state["exit_code"] == 2


def test_skip_on_running_child_refuses(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        sandbox, tmp_path, status="running", exit_code=None
    )
    ok = _ack.do_skip("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_skip_on_completed_child_refuses(sandbox, tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        sandbox, tmp_path, status="completed", exit_code=0
    )
    ok = _ack.do_skip("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_print_table_header_and_row_shape(capsys):
    state = {
        "kind": "localgremlin",
        "stage": "implement",
        "started_at": "",
        "client": "claude",
    }
    row = _render.build_row("gr-test-aabb12", "/sf", "/wdir", state, "running")
    _render.print_table([row])
    out = capsys.readouterr().out.splitlines()
    assert len(out) == 2
    header = out[0]
    for col in ("KIND", "ID", "STAGE", "LIVENESS", "AGE", "CLIENT", "DESCRIPTION"):
        assert col in header
    data_row = out[1]
    assert "localgremlin" in data_row
    assert "running" in data_row
    assert "claude" in data_row


# ---------------------------------------------------------------------------
# Misc small-surface helpers
# ---------------------------------------------------------------------------


def test_parse_duration():
    assert _duration.parse_duration("30s") == 30
    assert _duration.parse_duration("5m") == 300
    assert _duration.parse_duration("2h") == 7200
    assert _duration.parse_duration("1d") == 86400


def test_parse_duration_invalid():
    with pytest.raises(ValueError):
        _duration.parse_duration("5x")
    with pytest.raises(ValueError):
        _duration.parse_duration("abc")


def test_parse_commit_output_strips_fences():
    assert _land._parse_commit_output("```\nfoo\n\nbar\n```") == ("foo", "bar")
    assert _land._parse_commit_output("```python\nfoo\n```") == ("foo", "")
    assert _land._parse_commit_output("foo\n\nbar") == ("foo", "bar")
    assert _land._parse_commit_output("```\nfoo\n```") == ("foo", "")
    assert _land._parse_commit_output("```") == ("", "")


# ---------------------------------------------------------------------------
# liveness — dead:host-terminated (pid gone + workdir missing)
# ---------------------------------------------------------------------------


def test_liveness_host_terminated_when_pid_gone_and_workdir_missing(tmp_path):
    workdir = tmp_path / "workdir"
    # workdir is NOT created — simulates host-terminated teardown
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": 999999, "workdir": str(workdir)},
    )
    assert _state.liveness_of_state_file(sf) == "dead:host-terminated"


def test_liveness_crashed_when_pid_gone_but_workdir_exists(tmp_path):
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": 999999, "workdir": str(workdir)},
    )
    live = _state.liveness_of_state_file(sf)
    assert live.startswith("dead:crashed")


def test_liveness_crashed_when_pid_gone_and_no_workdir_in_state(tmp_path):
    sf = _write_state(
        tmp_path / "g",
        {"status": "running", "pid": 999999},
    )
    live = _state.liveness_of_state_file(sf)
    assert live.startswith("dead:crashed")


@pytest.mark.parametrize(
    "label,state,artifacts,expected_land_fn",
    [
        (
            "empty_with_workdir",
            {
                "kind": "bossgremlin",
                "setup_kind": "worktree-detached",
                "id": "x",
            },
            [],
            "_land_boss",
        ),
        (
            "one_pr",
            {
                "kind": "ghgremlin",
                "id": "x",
                "artifacts": [
                    {"type": "branch", "name": "feat"},
                    {
                        "type": "pr",
                        "url": "https://github.com/o/r/pull/1",
                        "branch": "feat",
                    },
                ],
            },
            [],
            "_land_gh",
        ),
    ],
    ids=lambda x: x if isinstance(x, str) else "",
)
def test_do_land_dispatches_to_correct_helper(
    label, state, artifacts, expected_land_fn, sandbox, tmp_path, monkeypatch, capsys
):
    gremlin_id = "test-dispatch-id"
    state_root = sandbox.state
    gr_dir = state_root / gremlin_id
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    full_state = {
        "id": gremlin_id,
        "status": "dead",
        "exit_code": 0,
        "workdir": str(workdir),
        "project_root": str(tmp_path / "project"),
        **state,
    }
    _write_state(gr_dir, full_state, finished=True)

    called = []

    def fake_land_boss(gremlin_id, sf, wdir, state, mode):
        called.append("_land_boss")
        return True

    def fake_land_gh(gremlin_id, wdir, state, force):
        called.append("_land_gh")
        return True

    monkeypatch.setattr(_land_mod, "_land_boss", fake_land_boss)
    monkeypatch.setattr(_land_mod, "_land_gh", fake_land_gh)

    ok = _land_mod.do_land(gremlin_id)
    assert ok is True
    assert called == [expected_land_fn], f"expected {expected_land_fn}, got {called}"


def test_land_gh_removes_worktree_before_gh_merge(sandbox, tmp_path, monkeypatch):
    """_remove_worktree must be called before gh pr merge so --delete-branch succeeds."""
    import types

    pr_url = "https://github.com/o/r/pull/42"
    gremlin_id = "gh-land-order-test12"

    state_root = sandbox.state
    gr_dir = state_root / gremlin_id
    gr_dir.mkdir(parents=True)
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "ghgremlin",
        "status": "dead",
        "exit_code": 0,
        "workdir": str(workdir),
        "project_root": str(tmp_path),
        "artifacts": [{"type": "pr", "url": pr_url, "branch": "feat"}],
    }
    (gr_dir / "state.json").write_text(json.dumps(state))

    monkeypatch.setattr(
        "gremlins.artifacts.registry.ArtifactRegistry.read",
        lambda self, key: {"url": pr_url, "number": 42, "branch": "feat"},
    )

    call_order: list[str] = []

    def fake_remove_worktree(wdir, state, cwd):
        call_order.append("_remove_worktree")

    def fake_proc_run(cmd, **kwargs):
        if "merge" in cmd:
            call_order.append("gh_merge")
            result = types.SimpleNamespace(returncode=0, stdout="", stderr="")
        elif "view" in cmd:
            result = types.SimpleNamespace(
                returncode=0,
                stdout='{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"","statusCheckRollup":[]}',
                stderr="",
            )
        else:
            result = types.SimpleNamespace(returncode=0, stdout="", stderr="")
        return result

    monkeypatch.setattr(_land, "_remove_worktree", fake_remove_worktree)
    monkeypatch.setattr(_land.proc, "run", fake_proc_run)
    monkeypatch.setattr(_land, "_fast_forward_main", lambda cwd: None)
    monkeypatch.setattr(_land, "_finalize_cleanup", lambda *a, **kw: None)

    ok = _land._land_gh(gremlin_id, str(gr_dir), state)
    assert ok is True
    assert call_order.index("_remove_worktree") < call_order.index("gh_merge")


# ---------------------------------------------------------------------------
# parse_liveness — structured liveness conversion
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "live,expected",
    [
        ("running", {"state": "running"}),
        ("finished", {"state": "finished"}),
        ("waiting", {"state": "waiting"}),
        ("waiting (3m12s)", {"state": "waiting", "duration": "3m12s"}),
        ("dead:exit 2", {"state": "dead", "reason": "exit", "exit_code": 2}),
        (
            "dead:bailed:structural",
            {"state": "dead", "reason": "bailed", "bail_reason": "structural"},
        ),
        ("dead:host-terminated", {"state": "dead", "reason": "host-terminated"}),
        ("dead:unknown", {"state": "dead", "reason": "unknown"}),
        (
            "dead:crashed (pid 123 gone)",
            {"state": "dead", "reason": "crashed", "detail": "(pid 123 gone)"},
        ),
        (
            "stalled:no log update 5m",
            {"state": "stalled", "detail": "no log update 5m"},
        ),
        ("", {"state": "unknown"}),
    ],
)
def test_parse_liveness(live, expected):
    assert _state.parse_liveness(live) == expected


# ---------------------------------------------------------------------------
# do_list_json — fleet list as JSON
# ---------------------------------------------------------------------------


def _make_args(**kwargs):
    """Build a minimal argparse.Namespace for do_list_json / do_list."""
    defaults = dict(
        running=False,
        dead=False,
        stalled=False,
        pipeline=None,
        since=None,
        recent=None,
        json=False,
    )
    defaults.update(kwargs)
    import argparse

    return argparse.Namespace(**defaults)


_EMPTY_QUEUE = {"pending": 0, "running": 0, "failed": 0, "runner_active": False}


def test_do_list_json_emits_gremlins_and_queue(sandbox, tmp_path, monkeypatch, capsys):
    state_root = sandbox.state
    gr_dir = state_root / "test-id-json01"
    _write_state(
        gr_dir,
        {
            "id": "test-id-json01",
            "kind": "localgremlin",
            "stage": "implement",
            "status": "running",
            "pid": os.getpid(),
            "client": "claude",
            "description": "test task",
            "started_at": "2024-01-01T00:00:00Z",
        },
        log_text="recent",
    )
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    out = capsys.readouterr().out
    data = json.loads(out)
    assert "gremlins" in data
    assert "queue" in data
    assert len(data["gremlins"]) == 1
    item = data["gremlins"][0]
    assert item["id"] == "test-id-json01"
    assert item["kind"] == "localgremlin"
    assert item["stage"] == "implement"
    assert item["client"] == "claude"
    assert item["description"] == "test task"
    assert isinstance(item["liveness"], dict)
    assert item["liveness"]["state"] == "running"
    assert item["age_seconds"] is not None


def test_do_list_json_dead_liveness_structured(sandbox, tmp_path, monkeypatch, capsys):
    state_root = sandbox.state
    gr_dir = state_root / "test-id-dead01"
    _write_state(
        gr_dir,
        {
            "id": "test-id-dead01",
            "kind": "localgremlin",
            "stage": "implement",
            "status": "dead",
            "exit_code": 1,
            "started_at": "2024-01-01T00:00:00Z",
        },
        finished=True,
    )
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    out = capsys.readouterr().out
    data = json.loads(out)
    assert data["gremlins"][0]["liveness"] == {
        "state": "dead",
        "reason": "exit",
        "exit_code": 1,
    }


def test_do_list_json_empty_fleet(sandbox, tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    out = capsys.readouterr().out
    data = json.loads(out)
    assert data["gremlins"] == []


# ---------------------------------------------------------------------------
# Queue header — do_list text output and do_list_json queue field
# ---------------------------------------------------------------------------


def test_do_list_json_queue_field_empty(sandbox, tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    data = json.loads(capsys.readouterr().out)
    assert data["queue"] == _EMPTY_QUEUE


def test_do_list_json_queue_field_with_items(sandbox, tmp_path, monkeypatch, capsys):
    summary = {"pending": 3, "running": 1, "failed": 0, "runner_active": True}
    monkeypatch.setattr(_views, "queue_summary", lambda: summary)
    _views.do_list_json(_make_args(), here_root=None)
    data = json.loads(capsys.readouterr().out)
    assert data["queue"] == summary


def test_do_list_shows_queue_header_with_active_runner(
    sandbox, tmp_path, monkeypatch, capsys
):
    monkeypatch.setattr(
        _views,
        "queue_summary",
        lambda: {"pending": 3, "running": 1, "failed": 0, "runner_active": True},
    )
    _views.do_list(_make_args(), here_root=None)
    out = capsys.readouterr().out
    assert "queue:" in out
    assert "pending 3" in out
    assert "running 1" in out
    assert "runner: active" in out
    assert "NOT RUNNING" not in out


def test_do_list_shows_loud_warning_when_runner_dead(
    sandbox, tmp_path, monkeypatch, capsys
):
    monkeypatch.setattr(
        _views,
        "queue_summary",
        lambda: {"pending": 3, "running": 0, "failed": 0, "runner_active": False},
    )
    _views.do_list(_make_args(), here_root=None)
    out = capsys.readouterr().out
    assert "NOT RUNNING" in out
    assert "3 items waiting" in out


def test_do_list_suppresses_queue_header_when_empty(
    sandbox, tmp_path, monkeypatch, capsys
):
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list(_make_args(), here_root=None)
    out = capsys.readouterr().out
    assert "queue:" not in out


# ---------------------------------------------------------------------------
# do_drill_in_json — single-gremlin JSON drill-in
# ---------------------------------------------------------------------------


def test_do_drill_in_json_emits_json_object(sandbox, tmp_path, monkeypatch, capsys):
    state_root = sandbox.state
    gr_dir = state_root / "test-id-drill1"
    _write_state(
        gr_dir,
        {
            "id": "test-id-drill1",
            "kind": "localgremlin",
            "stage": "implement",
            "status": "dead",
            "exit_code": 2,
            "started_at": "2024-01-01T00:00:00Z",
        },
        finished=True,
    )
    _views.do_drill_in_json("test-id-drill1")
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert obj["id"] == "test-id-drill1"
    assert obj["liveness"] == {"state": "dead", "reason": "exit", "exit_code": 2}
    assert obj["closed"] is False
    assert "state" in obj
    assert obj["state"]["kind"] == "localgremlin"
    assert obj["artifact_paths"] == []


def test_do_drill_in_json_no_match(sandbox, tmp_path, monkeypatch, capsys):
    _views.do_drill_in_json("nonexistent")
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert "error" in obj
    assert "no gremlin matched" in obj["error"]


def test_do_drill_in_json_includes_log_path(sandbox, tmp_path, monkeypatch, capsys):
    state_root = sandbox.state
    gr_dir = state_root / "test-id-log001"
    _write_state(
        gr_dir,
        {
            "id": "test-id-log001",
            "kind": "localgremlin",
            "stage": "implement",
            "status": "dead",
            "exit_code": 0,
        },
        finished=True,
        log_text="some output",
    )
    _views.do_drill_in_json("test-id-log001")
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert obj["log_path"] is not None
    assert obj["log_path"].endswith("log")


# ---------------------------------------------------------------------------
# fleet CLI --json flag routing
# ---------------------------------------------------------------------------


def test_cli_fleet_json_no_state_root(sandbox, tmp_path, monkeypatch, capsys):
    sandbox.state.rmdir()
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    monkeypatch.setattr(_fleet_cli, "queue_summary", lambda: _EMPTY_QUEUE)
    with pytest.raises(SystemExit) as exc:
        _main_impl(["--json"])
    assert exc.value.code == 0
    out = capsys.readouterr().out
    assert json.loads(out)["gremlins"] == []


def test_cli_fleet_json_drill_in_no_state_root(sandbox, tmp_path, monkeypatch, capsys):
    sandbox.state.rmdir()
    with pytest.raises(SystemExit) as exc:
        _main_impl(["gr-abc", "--json"])
    assert exc.value.code == 0
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert "error" in obj


def test_cli_fleet_json_list(sandbox, tmp_path, monkeypatch, capsys):
    state_root = sandbox.state
    gr_dir = state_root / "test-cli-json01"
    _write_state(
        gr_dir,
        {
            "id": "test-cli-json01",
            "kind": "localgremlin",
            "stage": "implement",
            "status": "running",
            "pid": os.getpid(),
            "started_at": "2024-01-01T00:00:00Z",
        },
        log_text="recent",
    )
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    with pytest.raises(SystemExit) as exc:
        _main_impl(["--json"])
    assert exc.value.code == 0
    data = json.loads(capsys.readouterr().out)
    assert "gremlins" in data
    assert data["gremlins"][0]["id"] == "test-cli-json01"


def test_cli_fleet_json_drill_in(sandbox, tmp_path, monkeypatch, capsys):
    state_root = sandbox.state
    gr_dir = state_root / "test-cli-drill01"
    _write_state(
        gr_dir,
        {
            "id": "test-cli-drill01",
            "kind": "localgremlin",
            "stage": "implement",
            "status": "dead",
            "exit_code": 0,
            "started_at": "2024-01-01T00:00:00Z",
        },
        finished=True,
    )
    with pytest.raises(SystemExit) as exc:
        _main_impl(["test-cli-drill01", "--json"])
    assert exc.value.code == 0
    obj = json.loads(capsys.readouterr().out)
    assert obj["id"] == "test-cli-drill01"
    assert isinstance(obj["liveness"], dict)


# ---------------------------------------------------------------------------
# _exec_land_stage
# ---------------------------------------------------------------------------


def test_exec_land_stage_success():
    from unittest.mock import MagicMock

    class _OkStage:
        async def run(self, gremlin):  # type: ignore[no-untyped-def]
            pass

    mock_gremlin = MagicMock()
    result = _land._exec_land_stage(_OkStage(), mock_gremlin)
    assert result is True


def test_exec_land_stage_bail(capsys):
    from unittest.mock import MagicMock

    from gremlins.stages.outcome import Bail

    class _BailStage:
        async def run(self, gremlin):  # type: ignore[no-untyped-def]
            raise Bail("structural")

    mock_gremlin = MagicMock()
    result = _land._exec_land_stage(_BailStage(), mock_gremlin)
    assert result is False
    assert "structural" in capsys.readouterr().out
