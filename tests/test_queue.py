"""Tests for gremlins queue."""

from __future__ import annotations

import json
import pathlib
import subprocess
from unittest.mock import MagicMock

import pytest

import gremlins.queue.core as core
from gremlins.cli import main


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def q(tmp_path, monkeypatch):
    """Patch state_root and return the queue root."""
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    root = core.queue_root()
    return root


# ---------------------------------------------------------------------------
# add
# ---------------------------------------------------------------------------


def test_add_creates_pending_file(q):
    name = core.add("echo hello")
    assert (q / "pending" / name).exists()
    assert (q / "pending" / name).read_text() == "echo hello"


def test_add_counter_increments(q):
    n1 = core.add("echo one")
    n2 = core.add("echo two")
    n3 = core.add("echo three")
    assert n1.startswith("0000-")
    assert n2.startswith("0001-")
    assert n3.startswith("0002-")


# ---------------------------------------------------------------------------
# run — empty / stale
# ---------------------------------------------------------------------------


def test_run_empty_queue_exits_zero(q):
    assert core.run() == 0


def test_run_refuses_stale_running(q, capsys):
    (q / "running" / "0000-item.cmd").write_text("echo hi")
    rc = core.run()
    assert rc == 1
    assert "stale" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# run — plain commands
# ---------------------------------------------------------------------------


def test_run_plain_cmd_success(q):
    core.add("true")
    rc = core.run()
    assert rc == 0
    done = list((q / "done").glob("*.cmd"))
    assert len(done) == 1


def test_run_plain_cmd_failure(q):
    core.add("false")
    rc = core.run()
    assert rc == 1
    failed = list((q / "failed").glob("*.cmd"))
    assert len(failed) == 1


def test_run_second_item_stays_pending_after_failure(q):
    core.add("false")
    core.add("echo second")
    core.run()
    pending = list((q / "pending").glob("*.cmd"))
    assert len(pending) == 1
    # second item's slug is derived from "echo" (first token), not "second"
    assert pending[0].name.startswith("0001-")


# ---------------------------------------------------------------------------
# run — launch commands
# ---------------------------------------------------------------------------


def test_run_launch_cmd_success(q, monkeypatch):
    fake_id = "gr-abc123"
    monkeypatch.setattr(
        "gremlins.queue.core._run_launch",
        lambda cmd, log_path: (fake_id, True),
    )
    monkeypatch.setattr(
        "gremlins.queue.core._poll_terminal",
        lambda gr_id: {"exit_code": 0, "status": "done"},
    )
    core.add("gremlins launch local")
    rc = core.run()
    assert rc == 0
    done = list((q / "done").glob("*.cmd"))
    assert len(done) == 1
    assert fake_id in done[0].name


def test_run_launch_cmd_dirty_state(q, monkeypatch):
    fake_id = "gr-dirty1"
    monkeypatch.setattr(
        "gremlins.queue.core._run_launch",
        lambda cmd, log_path: (fake_id, True),
    )
    monkeypatch.setattr(
        "gremlins.queue.core._poll_terminal",
        lambda gr_id: {"exit_code": 0, "status": "bailed", "bail_class": "security"},
    )
    core.add("gremlins launch local")
    rc = core.run()
    assert rc == 1
    failed = list((q / "failed").glob("*.cmd"))
    assert len(failed) == 1


def test_run_launch_cmd_proc_failure(q, monkeypatch):
    monkeypatch.setattr(
        "gremlins.queue.core._run_launch",
        lambda cmd, log_path: (None, False),
    )
    core.add("gremlins launch local")
    rc = core.run()
    assert rc == 1
    failed = list((q / "failed").glob("*.cmd"))
    assert len(failed) == 1


def test_run_ctrl_c_leaves_item_in_running(q, monkeypatch):
    fake_id = "gr-ctrlc1"
    monkeypatch.setattr(
        "gremlins.queue.core._run_launch",
        lambda cmd, log_path: (fake_id, True),
    )
    monkeypatch.setattr(
        "gremlins.queue.core._poll_terminal",
        lambda gr_id: (_ for _ in ()).throw(KeyboardInterrupt()),
    )
    core.add("gremlins launch local")
    try:
        core.run()
    except KeyboardInterrupt:
        pass
    running = list((q / "running").glob("*.cmd"))
    assert len(running) == 1


# ---------------------------------------------------------------------------
# list_queue
# ---------------------------------------------------------------------------


def test_list_empty_queue(q, capsys):
    core.list_queue()
    assert "(queue is empty)" in capsys.readouterr().out


def test_list_shows_items_in_all_buckets(q, capsys):
    (q / "pending" / "0000-alpha.cmd").write_text("echo a")
    (q / "done" / "0001-beta.cmd").write_text("echo b")
    (q / "failed" / "0002-gamma.cmd").write_text("echo c")
    core.list_queue()
    out = capsys.readouterr().out
    assert "alpha" in out
    assert "beta" in out
    assert "gamma" in out


def test_list_shows_gremlin_id(q, capsys):
    (q / "done" / "0000-local.gr-testid1.cmd").write_text("gremlins launch local")
    core.list_queue()
    out = capsys.readouterr().out
    assert "[gr-testid1]" in out


# ---------------------------------------------------------------------------
# requeue
# ---------------------------------------------------------------------------


def test_requeue_moves_failed_to_pending(q):
    (q / "failed" / "0000-item.cmd").write_text("echo x")
    core.requeue()
    assert (q / "pending" / "0000-item.cmd").exists()
    assert not (q / "failed" / "0000-item.cmd").exists()


def test_requeue_with_done_flag(q):
    (q / "failed" / "0000-item.cmd").write_text("echo x")
    (q / "done" / "0001-item.cmd").write_text("echo y")
    core.requeue(include_done=True)
    assert (q / "pending" / "0000-item.cmd").exists()
    assert (q / "pending" / "0001-item.cmd").exists()


def test_requeue_moves_log_sidecar(q):
    (q / "failed" / "0000-item.cmd").write_text("echo x")
    (q / "failed" / "0000-item.log").write_text("log output")
    core.requeue()
    assert (q / "pending" / "0000-item.log").exists()
    assert not (q / "failed" / "0000-item.log").exists()


# ---------------------------------------------------------------------------
# clear
# ---------------------------------------------------------------------------


def test_clear_deletes_done_and_failed(q):
    (q / "done" / "0000-a.cmd").write_text("echo a")
    (q / "failed" / "0001-b.cmd").write_text("echo b")
    core.clear()
    assert not list((q / "done").glob("*.cmd"))
    assert not list((q / "failed").glob("*.cmd"))


def test_clear_failed_only(q):
    (q / "done" / "0000-a.cmd").write_text("echo a")
    (q / "failed" / "0001-b.cmd").write_text("echo b")
    core.clear(failed_only=True)
    assert (q / "done" / "0000-a.cmd").exists()
    assert not list((q / "failed").glob("*.cmd"))


def test_clear_done_only(q):
    (q / "done" / "0000-a.cmd").write_text("echo a")
    (q / "failed" / "0001-b.cmd").write_text("echo b")
    core.clear(done_only=True)
    assert not list((q / "done").glob("*.cmd"))
    assert (q / "failed" / "0001-b.cmd").exists()


def test_clear_purge_empties_all(q):
    for sub in ("pending", "running", "done", "failed"):
        (q / sub / "0000-x.cmd").write_text("echo x")
    core.clear(purge=True)
    for sub in ("pending", "running", "done", "failed"):
        assert not list((q / sub).glob("*.cmd"))


def test_clear_purge_stops_running_gremlin(q, monkeypatch):
    stopped = []
    monkeypatch.setattr(
        "gremlins.queue.core.subprocess.run",
        lambda cmd, **kw: stopped.append(cmd) or MagicMock(returncode=0),
    )
    (q / "running" / "0000-local.gr-run001.cmd").write_text("gremlins launch local")
    core.clear(purge=True)
    assert any("gr-run001" in str(c) for c in stopped)


# ---------------------------------------------------------------------------
# land
# ---------------------------------------------------------------------------


def test_land_success(q, monkeypatch):
    calls = []
    monkeypatch.setattr(
        "gremlins.queue.core.subprocess.run",
        lambda cmd, **kw: calls.append(cmd) or MagicMock(returncode=0),
    )
    (q / "done" / "0000-local.gr-land01.cmd").write_text("gremlins launch local")
    (q / "done" / "0001-local.gr-land02.cmd").write_text("gremlins launch local")
    rc = core.land()
    assert rc == 0
    ids = [c[-1] for c in calls]
    assert "gr-land01" in ids
    assert "gr-land02" in ids


def test_land_halts_on_failure(q, monkeypatch):
    call_count = [0]

    def fake_run(cmd, **kw):
        call_count[0] += 1
        return MagicMock(returncode=1)

    monkeypatch.setattr("gremlins.queue.core.subprocess.run", fake_run)
    (q / "done" / "0000-local.gr-fail01.cmd").write_text("gremlins launch local")
    (q / "done" / "0001-local.gr-fail02.cmd").write_text("gremlins launch local")
    rc = core.land()
    assert rc == 1
    assert call_count[0] == 1


def test_land_skips_non_launch_items(q, monkeypatch):
    calls = []
    monkeypatch.setattr(
        "gremlins.queue.core.subprocess.run",
        lambda cmd, **kw: calls.append(cmd) or MagicMock(returncode=0),
    )
    # No id in filename — should be skipped
    (q / "done" / "0000-plain.cmd").write_text("echo hello")
    rc = core.land()
    assert rc == 0
    assert calls == []


# ---------------------------------------------------------------------------
# CLI dispatch
# ---------------------------------------------------------------------------


def test_cli_queue_add_dispatches(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "add", "echo hello"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "queued:" in out


def test_cli_queue_list_dispatches(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "list"])
    assert rc == 0
    assert "(queue is empty)" in capsys.readouterr().out


def test_cli_queue_run_dispatches(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "run"])
    assert rc == 0


def test_cli_queue_requeue_dispatches(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "requeue"])
    assert rc == 0


def test_cli_queue_clear_dispatches(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "clear"])
    assert rc == 0


def test_cli_queue_land_dispatches(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "land"])
    assert rc == 0
