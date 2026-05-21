"""Tests for gremlins/fleet.py."""

import json
import os
import pathlib
import subprocess

import pytest

import gremlins.cli.fleet as _fleet_cli
import gremlins.fleet.ack as _ack
import gremlins.fleet.close as _close
import gremlins.fleet.constants as _constants
import gremlins.fleet.duration as _duration
import gremlins.fleet.land as _land
import gremlins.fleet.land as _land_mod
import gremlins.fleet.render as _render
import gremlins.fleet.rescue as _rescue
import gremlins.fleet.rescue as _rescue_mod
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
    tmp_path, monkeypatch, gremlin_id="test-id-aabb12", **state_overrides
):
    """Build a state-root with a single dead gremlin, monkeypatch STATE_ROOT."""
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
        "rescue_count": 0,
    }
    state.update(state_overrides)
    _write_state(gr_dir, state, finished=True)
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
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


# ---------------------------------------------------------------------------
# build_row — rescue marker (state transition: dead → rescued → running)
# ---------------------------------------------------------------------------


def test_build_row_rescue_suffix_singular():
    state = {
        "kind": "localgremlin",
        "stage": "implement",
        "rescue_count": 1,
        "started_at": "",
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert "(rescue)" in row.liveness


def test_build_row_rescue_suffix_multiple():
    state = {
        "kind": "localgremlin",
        "stage": "implement",
        "rescue_count": 3,
        "started_at": "",
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert "(rescue x3)" in row.liveness


def test_build_row_no_rescue_suffix_when_zero():
    state = {
        "kind": "localgremlin",
        "stage": "implement",
        "rescue_count": 0,
        "started_at": "",
    }
    row = _render.build_row("g1", "/sf", "/wdir", state, "running")
    assert "(rescue" not in row.liveness


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
# Phase A marker contract — _read_rescue_marker
# ---------------------------------------------------------------------------


def _write_marker(path: pathlib.Path, payload) -> str:
    if isinstance(payload, str):
        path.write_text(payload)
    else:
        path.write_text(json.dumps(payload))
    return str(path)


def test_marker_missing_file(tmp_path):
    status, msg = _rescue._read_rescue_marker(str(tmp_path / "missing.json"))
    assert status == "no_marker"
    assert "did not write" in msg


def test_marker_unparseable(tmp_path):
    p = _write_marker(tmp_path / "m.json", "not json")
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "bad_marker"
    assert "unreadable" in msg


def test_marker_not_a_json_object(tmp_path):
    p = _write_marker(tmp_path / "m.json", [1, 2, 3])
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "bad_marker"
    assert "not a JSON object" in msg


def test_marker_invalid_status(tmp_path):
    p = _write_marker(tmp_path / "m.json", {"status": "bogus"})
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "bad_marker"
    assert "invalid status" in msg


def test_marker_summary_must_be_string(tmp_path):
    p = _write_marker(tmp_path / "m.json", {"status": "fixed", "summary": [1, 2]})
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "bad_marker"


def test_marker_fixed(tmp_path):
    p = _write_marker(
        tmp_path / "m.json", {"status": "fixed", "summary": "patched state.json"}
    )
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "fixed"
    assert msg == "patched state.json"


def test_marker_transient(tmp_path):
    p = _write_marker(
        tmp_path / "m.json", {"status": "transient", "summary": "network flake"}
    )
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "transient"
    assert msg == "network flake"


def test_marker_structural_with_summary(tmp_path):
    p = _write_marker(
        tmp_path / "m.json", {"status": "structural", "summary": "bug in foo.sh"}
    )
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "structural"
    assert msg == "bug in foo.sh"


def test_marker_structural_without_summary_uses_fallback(tmp_path):
    p = _write_marker(tmp_path / "m.json", {"status": "structural"})
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "structural"
    assert msg  # non-empty fallback
    assert "structural" in msg.lower()


def test_marker_unsalvageable_without_summary_uses_fallback(tmp_path):
    p = _write_marker(tmp_path / "m.json", {"status": "unsalvageable"})
    status, msg = _rescue._read_rescue_marker(p)
    assert status == "unsalvageable"
    assert "unsalvageable" in msg.lower()


def test_marker_summary_collapses_whitespace(tmp_path):
    p = _write_marker(
        tmp_path / "m.json", {"status": "fixed", "summary": "line one\nline two\t  end"}
    )
    status, msg = _rescue._read_rescue_marker(p)
    assert "\n" not in msg
    assert "line one" in msg and "line two" in msg


def test_marker_summary_capped_to_500_chars(tmp_path):
    p = _write_marker(tmp_path / "m.json", {"status": "fixed", "summary": "x" * 1000})
    status, msg = _rescue._read_rescue_marker(p)
    assert len(msg) <= 500
    assert msg.endswith("...")


# ---------------------------------------------------------------------------
# rescue --headless: bail-class exclusion
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "bail_class",
    [
        "reviewer_requested_changes",
        "security",
        "secrets",
    ],
)
def test_rescue_headless_excludes_class(tmp_path, monkeypatch, capsys, bail_class):
    gr_dir, _ = _setup_dead_gremlin(
        tmp_path,
        monkeypatch,
        bail_class=bail_class,
        bail_detail="upstream-set detail",
    )
    ok = _rescue.do_rescue("test-id-aabb12", headless=True)
    assert ok is False
    new = json.loads((gr_dir / "state.json").read_text())
    assert new["bail_reason"] == f"excluded_class:{bail_class}"
    assert new["bail_detail"] == "upstream-set detail"
    assert new["status"] == "bailed"
    assert (gr_dir / "finished").exists()


def test_rescue_headless_does_not_exclude_other_class(tmp_path, monkeypatch):
    """`other` is the only attempted class — verify it gets past the exclusion check."""
    gr_dir, _ = _setup_dead_gremlin(tmp_path, monkeypatch, bail_class="other")
    # Stub the diagnosis step so the rescue terminates without claude.
    monkeypatch.setattr(
        _rescue, "_run_headless_diagnosis", lambda *a, **kw: ("structural", "fake")
    )
    ok = _rescue.do_rescue("test-id-aabb12", headless=True)
    assert ok is False
    new = json.loads((gr_dir / "state.json").read_text())
    # Should bail with "structural" (from the stubbed diagnosis), NOT excluded_class
    assert new["bail_reason"] == "structural"


# ---------------------------------------------------------------------------
# rescue --headless: attempt cap enforcement
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("rescue_count", [3, 4, 10])
def test_rescue_headless_at_or_above_cap_refuses(tmp_path, monkeypatch, rescue_count):
    gr_dir, _ = _setup_dead_gremlin(tmp_path, monkeypatch, rescue_count=rescue_count)
    ok = _rescue.do_rescue("test-id-aabb12", headless=True)
    assert ok is False
    new = json.loads((gr_dir / "state.json").read_text())
    assert new["bail_reason"] == "attempts_exhausted"
    assert f"reached cap of {_constants.RESCUE_CAP}" in new["bail_detail"]


def test_rescue_headless_below_cap_proceeds_past_check(tmp_path, monkeypatch):
    gr_dir, _ = _setup_dead_gremlin(
        tmp_path, monkeypatch, rescue_count=_constants.RESCUE_CAP - 1
    )
    # Stub diagnosis so we don't actually run claude
    monkeypatch.setattr(
        _rescue,
        "_run_headless_diagnosis",
        lambda *a, **kw: ("structural", "agent flagged"),
    )
    ok = _rescue.do_rescue("test-id-aabb12", headless=True)
    assert ok is False
    new = json.loads((gr_dir / "state.json").read_text())
    # Bails with "structural" from diagnosis, not "attempts_exhausted"
    assert new["bail_reason"] == "structural"


def test_rescue_headless_running_refused(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(tmp_path, monkeypatch)
    # Mark as running
    state = json.loads((gr_dir / "state.json").read_text())
    state["status"] = "running"
    state["pid"] = os.getpid()
    state["exit_code"] = None
    (gr_dir / "state.json").write_text(json.dumps(state))
    (gr_dir / "finished").unlink()
    (gr_dir / "log").write_text("recent")

    ok = _rescue.do_rescue("test-id-aabb12", headless=True)
    assert ok is False
    out = capsys.readouterr().out
    assert "still running" in out


# ---------------------------------------------------------------------------
# do_close — close flow
# ---------------------------------------------------------------------------


def test_close_dead_gremlin_marks_closed(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(tmp_path, monkeypatch)
    ok = _close.do_close("test-id-aabb12")
    assert ok is True
    assert (gr_dir / "closed").exists()


def test_close_already_closed_is_idempotent(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(tmp_path, monkeypatch)
    (gr_dir / "closed").touch()
    ok = _close.do_close("test-id-aabb12")
    assert ok is True
    assert "already closed" in capsys.readouterr().out


def test_close_running_refused(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_dead_gremlin(tmp_path, monkeypatch)
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


def test_close_not_found(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    ok = _close.do_close("nonexistent-id")
    assert ok is False
    assert "no gremlin matched" in capsys.readouterr().out


# ---------------------------------------------------------------------------
# do_ack / do_skip
# ---------------------------------------------------------------------------


def _setup_bailed_gremlin(
    tmp_path, monkeypatch, gremlin_id="test-id-aabb12", **state_overrides
):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    return gr_dir, workdir


def test_ack_on_bailed_child_sets_external_outcome_landed(
    tmp_path, monkeypatch, capsys
):
    gr_dir, _ = _setup_bailed_gremlin(tmp_path, monkeypatch)
    ok = _ack.do_ack("test-id-aabb12")
    assert ok is True
    state = json.loads((gr_dir / "state.json").read_text())
    assert state["external_outcome"] == "landed"
    # other fields untouched
    assert state["status"] == "bailed"
    assert state["bail_reason"] == "reviewer_requested_changes"
    assert state["exit_code"] == 2


def test_ack_on_running_child_refuses(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        tmp_path, monkeypatch, status="running", exit_code=None
    )
    ok = _ack.do_ack("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_ack_on_completed_child_refuses(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        tmp_path, monkeypatch, status="completed", exit_code=0
    )
    ok = _ack.do_ack("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_skip_on_bailed_child_sets_external_outcome_abandoned(
    tmp_path, monkeypatch, capsys
):
    gr_dir, _ = _setup_bailed_gremlin(tmp_path, monkeypatch)
    ok = _ack.do_skip("test-id-aabb12")
    assert ok is True
    state = json.loads((gr_dir / "state.json").read_text())
    assert state["external_outcome"] == "abandoned"
    # other fields untouched
    assert state["status"] == "bailed"
    assert state["bail_reason"] == "reviewer_requested_changes"
    assert state["exit_code"] == 2


def test_skip_on_running_child_refuses(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        tmp_path, monkeypatch, status="running", exit_code=None
    )
    ok = _ack.do_skip("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


def test_skip_on_completed_child_refuses(tmp_path, monkeypatch, capsys):
    gr_dir, _ = _setup_bailed_gremlin(
        tmp_path, monkeypatch, status="completed", exit_code=0
    )
    ok = _ack.do_skip("test-id-aabb12")
    assert ok is False
    assert "not bailed" in capsys.readouterr().err
    state = json.loads((gr_dir / "state.json").read_text())
    assert "external_outcome" not in state


# ---------------------------------------------------------------------------
# do_land / _land_local — squash path
# ---------------------------------------------------------------------------


def test_land_local_squash_lands_branch_and_deletes_it(tmp_path, monkeypatch, capsys):
    project_root = tmp_path / "project"
    project_root.mkdir()
    _init_git_repo(project_root)

    branch = "bg/local/test-id-aabb12"
    subprocess.run(
        ["git", "checkout", "-b", branch],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    (project_root / "feature.txt").write_text("feature work\n")
    subprocess.run(
        ["git", "add", "."], cwd=project_root, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add feature.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "checkout", "main"], cwd=project_root, check=True, capture_output=True
    )

    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-aabb12"
    gr_dir = state_root / gremlin_id
    artifacts_dir = gr_dir / "artifacts"
    artifacts_dir.mkdir(parents=True)
    (artifacts_dir / "plan.md").write_text(
        "# Add feature\n\n## Context\nAdd feature.txt to the repo.\n"
    )
    workdir = tmp_path / "workdir"  # not actually a worktree; a stand-in
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "status": "dead",
        "exit_code": 0,
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": branch}],
        "workdir": str(workdir),
        "project_root": str(project_root),
    }
    _write_state(gr_dir, state, finished=True)

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _land,
        "_synthesize_commit_message_ai",
        lambda inputs: (
            "Add feature.txt to repo",
            "Adds feature.txt with placeholder content.",
            0.0,
        ),
    )
    monkeypatch.chdir(project_root)

    ok = _land._land_local(
        gremlin_id, str(gr_dir / "state.json"), str(gr_dir), state, mode="squash"
    )
    assert ok is True

    log_out = subprocess.run(
        ["git", "log", "--oneline", "main"],
        cwd=project_root,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    assert "Add feature.txt to repo" in log_out

    branches = subprocess.run(
        ["git", "branch", "--list", branch],
        cwd=project_root,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    assert branches.strip() == ""


def test_land_local_squash_folds_commit_synthesis_cost_into_total(
    tmp_path, monkeypatch, capsys
):
    """Squash-land must add the commit-message `claude -p` cost to total_cost_usd
    so the printed total — and the persisted state — cover land-time spend."""
    project_root = tmp_path / "project"
    project_root.mkdir()
    _init_git_repo(project_root)

    branch = "bg/local/test-id-cost12"
    subprocess.run(
        ["git", "checkout", "-b", branch],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    (project_root / "feature.txt").write_text("feature work\n")
    subprocess.run(
        ["git", "add", "."], cwd=project_root, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add feature.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "checkout", "main"], cwd=project_root, check=True, capture_output=True
    )

    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-cost12"
    gr_dir = state_root / gremlin_id
    artifacts_dir = gr_dir / "artifacts"
    artifacts_dir.mkdir(parents=True)
    (artifacts_dir / "plan.md").write_text(
        "# Add feature\n\n## Context\nAdd feature.txt to the repo.\n"
    )
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "status": "dead",
        "exit_code": 0,
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": branch}],
        "workdir": str(workdir),
        "project_root": str(project_root),
        "total_cost_usd": 1.0,
    }
    sf_path = _write_state(gr_dir, state, finished=True)

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _land,
        "_synthesize_commit_message_ai",
        lambda inputs: ("Add feature.txt to repo", "", 0.05),
    )
    monkeypatch.chdir(project_root)

    ok = _land._land_local(gremlin_id, sf_path, str(gr_dir), state, mode="squash")
    assert ok is True

    persisted = json.loads(pathlib.Path(sf_path).read_text())
    assert persisted["total_cost_usd"] == pytest.approx(1.05)
    assert "total cost: $1.0500" in capsys.readouterr().out


def test_land_local_refuses_non_worktree_branch_setup(tmp_path, monkeypatch, capsys):
    state = {
        "id": "x",
        "kind": "localgremlin",
        "setup_kind": "cp-snapshot",  # not worktree-branch
        "branch": "bg/local/x",
    }
    ok = _land._land_local("x", "/sf", "/wdir", state, mode="squash")
    assert ok is False
    assert "only worktree-branch gremlins" in capsys.readouterr().out


def test_land_local_refuses_when_branch_missing_from_state(
    tmp_path, monkeypatch, capsys
):
    state = {
        "id": "x",
        "kind": "localgremlin",
        "setup_kind": "worktree-branch",
    }
    ok = _land._land_local("x", "/sf", "/wdir", state, mode="squash")
    assert ok is False
    assert "no branch artifact" in capsys.readouterr().out


def test_land_local_into_dir_nonexistent_fails(tmp_path, monkeypatch, capsys):
    """_land_local returns False and prints an error when into_dir does not exist."""
    state = {
        "id": "x",
        "kind": "localgremlin",
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": "bg/local/x"}],
        "project_root": str(tmp_path / "project"),
    }
    ok = _land._land_local(
        "x",
        "/sf",
        "/wdir",
        state,
        mode="squash",
        into_dir=str(tmp_path / "nonexistent"),
    )
    assert ok is False
    assert "--into directory does not exist" in capsys.readouterr().out


def test_land_local_into_dir_lands_in_worktree(tmp_path, monkeypatch, capsys):
    """When into_dir is provided, the squash commit lands there instead of project_root."""
    project_root = tmp_path / "project"
    project_root.mkdir()
    _init_git_repo(project_root)

    gremlin_id = "land-into-wt-12345678"
    branch = f"bg/local/{gremlin_id}"

    # Create feature branch with a commit.
    subprocess.run(
        ["git", "checkout", "-b", branch],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    (project_root / "wt_feature.txt").write_text("from worktree\n")
    subprocess.run(
        ["git", "add", "wt_feature.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "commit", "-m", "add wt_feature.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "checkout", "main"], cwd=project_root, check=True, capture_output=True
    )

    # Create a detached worktree from main (simulating the boss worktree).
    into_dir = tmp_path / "boss_worktree"
    subprocess.run(
        ["git", "worktree", "add", "--detach", str(into_dir), "HEAD"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )

    state_root = tmp_path / "state-root"
    gr_dir = state_root / gremlin_id
    artifacts_dir = gr_dir / "artifacts"
    artifacts_dir.mkdir(parents=True)
    (artifacts_dir / "plan.md").write_text(
        "# Add wt_feature\n\n## Context\nAdd wt_feature.txt.\n"
    )
    workdir = tmp_path / "child_worktree"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "status": "dead",
        "exit_code": 0,
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": branch}],
        "workdir": str(workdir),
        "project_root": str(project_root),
        "total_cost_usd": 1.0,
    }
    sf = str(gr_dir / "state.json")
    pathlib.Path(sf).write_text(json.dumps(state))

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _land,
        "_synthesize_commit_message_ai",
        lambda inputs: ("Add wt_feature.txt", "", 0.0),
    )
    monkeypatch.chdir(into_dir)

    ok = _land._land_local(
        gremlin_id, sf, str(gr_dir), state, mode="squash", into_dir=str(into_dir)
    )
    assert ok is True

    # Feature must appear in the boss worktree, not in project_root.
    assert (into_dir / "wt_feature.txt").exists()
    assert not (project_root / "wt_feature.txt").exists()


def test_land_proceeds_with_untracked_files_present(tmp_path, monkeypatch, capsys):
    """Untracked files must not block land (they can't be clobbered by squash merge)."""
    project_root = tmp_path / "project"
    project_root.mkdir()
    _init_git_repo(project_root)

    branch = "bg/local/test-id-untr12"
    subprocess.run(
        ["git", "checkout", "-b", branch],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    (project_root / "feature.txt").write_text("feature work\n")
    subprocess.run(
        ["git", "add", "."], cwd=project_root, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add feature.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "checkout", "main"], cwd=project_root, check=True, capture_output=True
    )

    # Drop an untracked file into the working tree on main.
    (project_root / "scratch.tmp").write_text("scratch\n")

    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-untr12"
    gr_dir = state_root / gremlin_id
    artifacts_dir = gr_dir / "artifacts"
    artifacts_dir.mkdir(parents=True)
    (artifacts_dir / "plan.md").write_text(
        "# Add feature\n\n## Context\nAdd feature.txt to the repo.\n"
    )
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "status": "dead",
        "exit_code": 0,
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": branch}],
        "workdir": str(workdir),
        "project_root": str(project_root),
    }
    _write_state(gr_dir, state, finished=True)

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _land,
        "_synthesize_commit_message_ai",
        lambda inputs: ("Add feature.txt to repo", "", 0.0),
    )
    monkeypatch.chdir(project_root)

    ok = _land._land_local(
        gremlin_id, str(gr_dir / "state.json"), str(gr_dir), state, mode="squash"
    )
    assert ok is True
    # Untracked file must still be present after land.
    assert (project_root / "scratch.tmp").exists()


def test_land_refuses_with_tracked_modifications(tmp_path, monkeypatch, capsys):
    """Staged or modified tracked files must still block land."""
    project_root = tmp_path / "project"
    project_root.mkdir()
    _init_git_repo(project_root)

    branch = "bg/local/test-id-dirty1"
    subprocess.run(
        ["git", "checkout", "-b", branch],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    (project_root / "feature.txt").write_text("feature work\n")
    subprocess.run(
        ["git", "add", "."], cwd=project_root, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add feature.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "checkout", "main"], cwd=project_root, check=True, capture_output=True
    )

    # Dirty the tracked README.md on main.
    (project_root / "README.md").write_text("modified\n")

    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-dirty1"
    gr_dir = state_root / gremlin_id
    (gr_dir / "artifacts").mkdir(parents=True)
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "status": "dead",
        "exit_code": 0,
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": branch}],
        "workdir": str(workdir),
        "project_root": str(project_root),
    }
    _write_state(gr_dir, state, finished=True)

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.chdir(project_root)

    ok = _land._land_local(
        gremlin_id, str(gr_dir / "state.json"), str(gr_dir), state, mode="squash"
    )
    assert ok is False
    assert "working tree is not clean" in capsys.readouterr().out


def test_squash_land_failure_preserves_untracked_files(tmp_path, monkeypatch):
    """git clean -fd must not run when untracked files existed before the merge."""
    project_root = tmp_path / "project"
    project_root.mkdir()
    _init_git_repo(project_root)

    # Create a conflicting branch: adds a file that will conflict with an
    # untracked file of the same name already present in the working tree.
    branch = "bg/local/test-id-conf12"
    subprocess.run(
        ["git", "checkout", "-b", branch],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    (project_root / "conflict.txt").write_text("from branch\n")
    subprocess.run(
        ["git", "add", "."], cwd=project_root, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add conflict.txt"],
        cwd=project_root,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "checkout", "main"], cwd=project_root, check=True, capture_output=True
    )

    # Drop an untracked file with the same name — this will cause the squash merge to fail.
    (project_root / "conflict.txt").write_text("pre-existing untracked\n")

    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-conf12"
    gr_dir = state_root / gremlin_id
    artifacts_dir = gr_dir / "artifacts"
    artifacts_dir.mkdir(parents=True)
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "status": "dead",
        "exit_code": 0,
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": branch}],
        "workdir": str(workdir),
        "project_root": str(project_root),
    }
    _write_state(gr_dir, state, finished=True)

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.chdir(project_root)

    # The merge will fail (untracked file would be overwritten), but the
    # pre-existing untracked file must survive — git clean -fd must not run.
    ok = _land._land_local(
        gremlin_id, str(gr_dir / "state.json"), str(gr_dir), state, mode="squash"
    )
    assert ok is False
    assert (project_root / "conflict.txt").read_text() == "pre-existing untracked\n"


# ---------------------------------------------------------------------------
# print_table — header and row shape
# ---------------------------------------------------------------------------


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


def test_atomic_patch_state_round_trip(tmp_path):
    sf = tmp_path / "state.json"
    sf.write_text(json.dumps({"a": 1, "b": 2}))
    ok = _rescue._atomic_patch_state(str(sf), {"b": 99, "c": 3})
    assert ok is True
    new = json.loads(sf.read_text())
    assert new == {"a": 1, "b": 99, "c": 3}


def test_atomic_patch_state_unreadable_file(tmp_path):
    ok = _rescue._atomic_patch_state(str(tmp_path / "missing.json"), {"a": 1})
    assert ok is False


def test_write_bail_marks_terminal(tmp_path):
    wdir = tmp_path / "wdir"
    wdir.mkdir()
    sf = wdir / "state.json"
    sf.write_text(json.dumps({"id": "x", "status": "dead", "exit_code": 2}))
    _rescue._write_bail(str(sf), str(wdir), "structural", "the agent said so")
    new = json.loads(sf.read_text())
    assert new["bail_reason"] == "structural"
    assert new["bail_detail"] == "the agent said so"
    assert new["status"] == "bailed"
    assert (wdir / "finished").exists()


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


# ---------------------------------------------------------------------------
# rescue — dead:host-terminated handling
# ---------------------------------------------------------------------------


def test_rescue_host_terminated_project_root_gone_bails_headless(
    tmp_path, monkeypatch, capsys
):
    """When project_root is also missing, headless rescue should bail with host_terminated_unrecoverable."""
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-htbb12"
    gr_dir = state_root / gremlin_id
    workdir = tmp_path / "workdir"
    # workdir is NOT created — simulates host teardown
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "stage": "implement",
        "status": "running",
        "pid": 999999,  # dead pid
        "workdir": str(workdir),
        "project_root": str(tmp_path / "gone-project"),  # also gone
        "rescue_count": 0,
    }
    _write_state(gr_dir, state)
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    ok = _rescue.do_rescue(gremlin_id, headless=True)
    assert ok is False
    new = json.loads((gr_dir / "state.json").read_text())
    assert new["bail_reason"] == "host_terminated_unrecoverable"
    assert (gr_dir / "finished").exists()
    out = capsys.readouterr().out
    assert "host" in out.lower() or "terminated" in out.lower() or "gone" in out.lower()


def test_rescue_host_terminated_worktree_recreation_failure_bails_headless(
    tmp_path, monkeypatch, capsys
):
    """When worktree recreation fails, headless rescue should bail with host_terminated_unrecoverable."""
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-htcc12"
    gr_dir = state_root / gremlin_id
    project_root = tmp_path / "project"
    project_root.mkdir()
    workdir = tmp_path / "workdir"
    # workdir NOT created
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "stage": "implement",
        "status": "running",
        "pid": 999999,
        "workdir": str(workdir),
        "project_root": str(project_root),
        "rescue_count": 0,
    }
    _write_state(gr_dir, state)
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _rescue, "recreate_worktree", lambda s: (False, "git not a repo")
    )

    ok = _rescue.do_rescue(gremlin_id, headless=True)
    assert ok is False
    new = json.loads((gr_dir / "state.json").read_text())
    assert new["bail_reason"] == "host_terminated_unrecoverable"
    assert (gr_dir / "finished").exists()


def test_rescue_host_terminated_recreates_worktree_and_proceeds(
    tmp_path, monkeypatch, capsys
):
    """When worktree recreation succeeds, rescue continues to the diagnosis step."""
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gremlin_id = "test-id-htdd12"
    gr_dir = state_root / gremlin_id
    project_root = tmp_path / "project"
    project_root.mkdir()
    workdir = tmp_path / "workdir"
    # workdir NOT created initially
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "stage": "implement",
        "status": "running",
        "pid": 999999,
        "workdir": str(workdir),
        "project_root": str(project_root),
        "rescue_count": 0,
    }
    _write_state(gr_dir, state)
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    def fake_recreate(s):
        workdir.mkdir(exist_ok=True)
        return True, "recreated from branch 'bg/local/test-id-htdd12'"

    monkeypatch.setattr(_rescue, "recreate_worktree", fake_recreate)
    monkeypatch.setattr(
        _rescue,
        "_run_headless_diagnosis",
        lambda *a, **kw: ("structural", "fake structural"),
    )

    ok = _rescue.do_rescue(gremlin_id, headless=True)
    assert ok is False  # structural bail, not host-terminated
    new = json.loads((gr_dir / "state.json").read_text())
    # Should have bailed with "structural", not "host_terminated_unrecoverable"
    assert new["bail_reason"] == "structural"
    out = capsys.readouterr().out
    assert "recreated" in out


@pytest.mark.parametrize(
    "label,state,artifacts,expected_land_fn",
    [
        (
            "one_branch",
            {
                "kind": "localgremlin",
                "setup_kind": "worktree-branch",
                "id": "x",
                "artifacts": [{"type": "branch", "name": "bg/local/x"}],
            },
            [],
            "_land_local",
        ),
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
    label, state, artifacts, expected_land_fn, tmp_path, monkeypatch, capsys
):
    gremlin_id = "test-dispatch-id"
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    called = []

    def fake_land_local(gremlin_id, sf, wdir, state, mode, **kw):
        called.append("_land_local")
        return True

    def fake_land_boss(gremlin_id, sf, wdir, state, mode):
        called.append("_land_boss")
        return True

    def fake_land_gh(gremlin_id, wdir, state, force):
        called.append("_land_gh")
        return True

    monkeypatch.setattr(_land_mod, "_land_local", fake_land_local)
    monkeypatch.setattr(_land_mod, "_land_boss", fake_land_boss)
    monkeypatch.setattr(_land_mod, "_land_gh", fake_land_gh)

    ok = _land_mod.do_land(gremlin_id)
    assert ok is True
    assert called == [expected_land_fn], f"expected {expected_land_fn}, got {called}"


def test_do_land_one_branch_routes_to_local(tmp_path, monkeypatch):
    """A gremlin with one branch artifact dispatches to _land_local."""
    gremlin_id = "custard-pipeline-id"
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    gr_dir = state_root / gremlin_id
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    state = {
        "id": gremlin_id,
        "kind": "custard",
        "status": "dead",
        "exit_code": 0,
        "workdir": str(workdir),
        "project_root": str(tmp_path / "project"),
        "setup_kind": "worktree-branch",
        "artifacts": [{"type": "branch", "name": "bg/local/custard-pipeline-id"}],
    }
    _write_state(gr_dir, state, finished=True)
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    called = []
    monkeypatch.setattr(
        _land_mod, "_land_local", lambda *a, **kw: called.append("_land_local") or True
    )
    monkeypatch.setattr(
        _land_mod, "_land_boss", lambda *a, **kw: called.append("_land_boss") or True
    )
    monkeypatch.setattr(
        _land_mod, "_land_gh", lambda *a, **kw: called.append("_land_gh") or True
    )

    ok = _land_mod.do_land(gremlin_id)
    assert ok is True
    assert called == ["_land_local"]


def test_land_gh_removes_worktree_before_gh_merge(tmp_path, monkeypatch):
    """_remove_worktree must be called before gh pr merge so --delete-branch succeeds."""
    import types

    pr_url = "https://github.com/o/r/pull/42"
    gremlin_id = "gh-land-order-test12"

    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
        "project_root": str(tmp_path / "project"),
        "artifacts": [{"type": "pr", "url": pr_url, "branch": "feat"}],
    }
    (gr_dir / "state.json").write_text(json.dumps(state))

    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        "gremlins.executor.state.StateData.read_pr_url", lambda self: pr_url
    )
    monkeypatch.setattr(_land, "_resolve_landing_cwd", lambda s: str(tmp_path))

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


def test_rescue_prompt_uses_pipeline_name():
    """build_rescue_prompt uses pipeline name from pipeline_path, not raw kind."""
    state = {
        "kind": "custard",
        "pipeline_path": "/some/path/boss.yaml",
        "stage": "chain",
        "description": "test",
        "project_root": "",
        "workdir": "",
        "parent_id": "",
    }
    prompt = _rescue_mod.build_rescue_prompt(state, "log", "/sf", "/log", "/marker")
    assert "boss" in prompt
    assert "custard" not in prompt.split("Pipeline:")[1].split("\n")[0]


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


def test_do_list_json_emits_gremlins_and_queue(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
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


def test_do_list_json_dead_liveness_structured(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    out = capsys.readouterr().out
    data = json.loads(out)
    assert data["gremlins"][0]["liveness"] == {"state": "dead", "reason": "exit", "exit_code": 1}


def test_do_list_json_empty_fleet(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    out = capsys.readouterr().out
    data = json.loads(out)
    assert data["gremlins"] == []


# ---------------------------------------------------------------------------
# Queue header — do_list text output and do_list_json queue field
# ---------------------------------------------------------------------------


def test_do_list_json_queue_field_empty(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list_json(_make_args(), here_root=None)
    data = json.loads(capsys.readouterr().out)
    assert data["queue"] == _EMPTY_QUEUE


def test_do_list_json_queue_field_with_items(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    summary = {"pending": 3, "running": 1, "failed": 0, "runner_active": True}
    monkeypatch.setattr(_views, "queue_summary", lambda: summary)
    _views.do_list_json(_make_args(), here_root=None)
    data = json.loads(capsys.readouterr().out)
    assert data["queue"] == summary


def test_do_list_shows_queue_header_with_active_runner(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _views, "queue_summary",
        lambda: {"pending": 3, "running": 1, "failed": 0, "runner_active": True},
    )
    _views.do_list(_make_args(), here_root=None)
    out = capsys.readouterr().out
    assert "queue:" in out
    assert "pending 3" in out
    assert "running 1" in out
    assert "runner: active" in out
    assert "NOT RUNNING" not in out


def test_do_list_shows_loud_warning_when_runner_dead(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(
        _views, "queue_summary",
        lambda: {"pending": 3, "running": 0, "failed": 0, "runner_active": False},
    )
    _views.do_list(_make_args(), here_root=None)
    out = capsys.readouterr().out
    assert "NOT RUNNING" in out
    assert "3 items waiting" in out


def test_do_list_suppresses_queue_header_when_empty(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    _views.do_list(_make_args(), here_root=None)
    out = capsys.readouterr().out
    assert "queue:" not in out


# ---------------------------------------------------------------------------
# do_drill_in_json — single-gremlin JSON drill-in
# ---------------------------------------------------------------------------


def test_do_drill_in_json_emits_json_object(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    _views.do_drill_in_json("test-id-drill1")
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert obj["id"] == "test-id-drill1"
    assert obj["liveness"] == {"state": "dead", "reason": "exit", "exit_code": 2}
    assert obj["closed"] is False
    assert "state" in obj
    assert obj["state"]["kind"] == "localgremlin"
    assert obj["artifact_paths"] == []
    assert obj["rescue_reports"] == []


def test_do_drill_in_json_no_match(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    _views.do_drill_in_json("nonexistent")
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert "error" in obj
    assert "no gremlin matched" in obj["error"]


def test_do_drill_in_json_includes_log_path(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    _views.do_drill_in_json("test-id-log001")
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert obj["log_path"] is not None
    assert obj["log_path"].endswith("log")


# ---------------------------------------------------------------------------
# fleet CLI --json flag routing
# ---------------------------------------------------------------------------


def test_cli_fleet_json_no_state_root(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(_constants, "STATE_ROOT", str(tmp_path / "nonexistent"))
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    monkeypatch.setattr(_fleet_cli, "queue_summary", lambda: _EMPTY_QUEUE)
    with pytest.raises(SystemExit) as exc:
        _main_impl(["--json"])
    assert exc.value.code == 0
    out = capsys.readouterr().out
    assert json.loads(out)["gremlins"] == []


def test_cli_fleet_json_drill_in_no_state_root(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(_constants, "STATE_ROOT", str(tmp_path / "nonexistent"))
    with pytest.raises(SystemExit) as exc:
        _main_impl(["gr-abc", "--json"])
    assert exc.value.code == 0
    out = capsys.readouterr().out
    obj = json.loads(out)
    assert "error" in obj


def test_cli_fleet_json_list(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    monkeypatch.setattr(_views, "queue_summary", lambda: _EMPTY_QUEUE)
    with pytest.raises(SystemExit) as exc:
        _main_impl(["--json"])
    assert exc.value.code == 0
    data = json.loads(capsys.readouterr().out)
    assert "gremlins" in data
    assert data["gremlins"][0]["id"] == "test-cli-json01"


def test_cli_fleet_json_drill_in(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state-root"
    state_root.mkdir()
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
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))
    with pytest.raises(SystemExit) as exc:
        _main_impl(["test-cli-drill01", "--json"])
    assert exc.value.code == 0
    obj = json.loads(capsys.readouterr().out)
    assert obj["id"] == "test-cli-drill01"
    assert isinstance(obj["liveness"], dict)
