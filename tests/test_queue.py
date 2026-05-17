"""Tests for gremlins queue."""

from __future__ import annotations

import json
import re
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


def test_add_produces_distinct_timestamp_filenames(q):
    n1 = core.add("echo one")
    n2 = core.add("echo two")
    n3 = core.add("echo three")
    ts_pat = re.compile(r"^\d{8}T\d{6}_\d{6}-")
    assert ts_pat.match(n1)
    assert ts_pat.match(n2)
    assert ts_pat.match(n3)
    assert len({n1, n2, n3}) == 3


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


def test_run_progress_events_on_stdout(q, capsys):
    core.add("true")
    assert core.run() == 0
    captured = capsys.readouterr()
    assert "queue: running" in captured.out
    assert "queue: done" in captured.out
    assert captured.err == ""


def test_run_failure_message_on_stderr(q, capsys):
    core.add("false")
    assert core.run() == 1
    captured = capsys.readouterr()
    assert "queue: failed" in captured.err
    assert "queue: failed" not in captured.out


def test_run_stale_error_on_stderr(q, capsys):
    (q / "running" / "0000-item.cmd").write_text("echo hi")
    assert core.run() == 1
    captured = capsys.readouterr()
    assert "queue: error" in captured.err
    assert "queue: error" not in captured.out


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
    assert "-echo.cmd" in pending[0].name


# ---------------------------------------------------------------------------
# slug derivation
# ---------------------------------------------------------------------------


def test_slug_token_launch_returns_pipeline_name():
    tokens = "gremlins launch gh-terse --description X --plan #1".split()
    assert core._slug_token(tokens) == "gh-terse"


def test_slug_token_non_launch_returns_first_token():
    assert core._slug_token("echo hello".split()) == "echo"


def test_slug_token_non_gremlins_launch_word_returns_first_token():
    assert core._slug_token("echo launch foo".split()) == "echo"


def test_slug_token_launch_no_pipeline_falls_back():
    assert core._slug_token("gremlins launch --flag".split()) == "item"


def test_add_launch_uses_pipeline_name_as_slug(q):
    name = core.add("gremlins launch gh-terse --description 'do something'")
    assert "-gh-terse" in name


def test_add_no_gremlin_id_omits_id_from_filename(q):
    name = core.add("gremlins launch gh-terse")
    assert name.endswith(".cmd")
    stem = name[: -len(".cmd")]
    assert "." not in stem


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


def test_list_reverse_chronological_order(q, capsys):
    (q / "pending" / "0001-second.cmd").write_text("echo b")
    (q / "done" / "0002-first.cmd").write_text("echo a")
    (q / "failed" / "0000-third.cmd").write_text("echo c")
    core.list_queue()
    lines = [line for line in capsys.readouterr().out.splitlines() if line.strip()]
    stems = [line.split()[1] for line in lines]
    assert stems == ["0002-first", "0001-second", "0000-third"]


def test_list_shows_gremlin_id(q, capsys):
    (q / "done" / "0000-local.gr-testid1.cmd").write_text("gremlins launch local")
    core.list_queue()
    out = capsys.readouterr().out
    assert "[gr-testid1]" in out


def test_list_shows_description(q, capsys):
    (q / "pending" / "0001-gh-terse.cmd").write_text(
        "gremlins launch gh-terse --description 'auto-generate CLI help'"
    )
    core.list_queue()
    assert "auto-generate CLI help" in capsys.readouterr().out


def test_list_no_crash_without_description(q, capsys):
    (q / "pending" / "0001-gh-terse.cmd").write_text("gremlins launch gh-terse")
    core.list_queue()
    out = capsys.readouterr().out
    assert "gh-terse" in out


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


def test_clear_pending_only(q):
    for sub in ("pending", "running", "done", "failed"):
        (q / sub / "0000-x.cmd").write_text("echo x")
    (q / "pending" / "0000-x.log").write_text("log")
    (q / "running" / "0000-x.log").write_text("log")
    core.clear(pending_only=True)
    assert not list((q / "pending").glob("*.cmd"))
    assert not list((q / "pending").glob("*.log"))
    for sub in ("running", "done", "failed"):
        assert list((q / sub).glob("*.cmd"))
    assert (q / "running" / "0000-x.log").exists()


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
# CLI dispatch
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# clear --item
# ---------------------------------------------------------------------------


def test_clear_item_pending(q):
    (q / "pending" / "0001-gh-terse.cmd").write_text("echo x")
    (q / "pending" / "0001-gh-terse.log").write_text("log")
    rc = core.clear(item="0001-gh-terse")
    assert rc == 0
    assert not (q / "pending" / "0001-gh-terse.cmd").exists()
    assert not (q / "pending" / "0001-gh-terse.log").exists()


def test_clear_item_done(q):
    (q / "done" / "0002-alpha.cmd").write_text("echo x")
    rc = core.clear(item="0002-alpha")
    assert rc == 0
    assert not (q / "done" / "0002-alpha.cmd").exists()


def test_clear_item_failed(q):
    (q / "failed" / "0003-beta.cmd").write_text("echo x")
    rc = core.clear(item="0003-beta")
    assert rc == 0
    assert not (q / "failed" / "0003-beta.cmd").exists()


def test_clear_item_running_refused(q, capsys):
    (q / "running" / "0004-live.cmd").write_text("echo x")
    rc = core.clear(item="0004-live")
    assert rc == 1
    assert "running" in capsys.readouterr().err


def test_clear_item_not_found(q, capsys):
    rc = core.clear(item="9999-ghost")
    assert rc == 1
    assert "no such item: 9999-ghost" in capsys.readouterr().err


def test_clear_item_multi_match(q, capsys):
    (q / "pending" / "0005-dup.cmd").write_text("echo x")
    (q / "failed" / "0005-dup.cmd").write_text("echo x")
    rc = core.clear(item="0005-dup")
    assert rc == 1
    err = capsys.readouterr().err
    assert "multiple" in err
    assert "pending" in err
    assert "failed" in err


def test_clear_item_mutex_with_pending(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    with pytest.raises(SystemExit):
        main(["queue", "clear", "--item", "foo", "--pending"])


def test_clear_item_mutex_with_purge(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    with pytest.raises(SystemExit):
        main(["queue", "clear", "--item", "foo", "--purge"])


def test_cli_queue_clear_flags_mutually_exclusive(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    import pytest

    with pytest.raises(SystemExit):
        main(["queue", "clear", "--failed", "--done"])


def test_cli_queue_add_dispatches(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "add", "echo hello"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "queued:" in out


def test_cli_queue_add_single_quoted_command_stored_verbatim(tmp_path, monkeypatch):
    """Single-element argv (quoted shell command) must be stored without extra escaping."""
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    cmd = "gremlins launch gh-terse --plan '#1' --description 'hi'"
    main(["queue", "add", cmd])
    pending = list((core.queue_root() / "pending").glob("*.cmd"))
    assert len(pending) == 1
    assert pending[0].read_text() == cmd


def test_cli_queue_add_multi_argv_shell_metacharacters_quoted(tmp_path, monkeypatch):
    """Multi-element argv with shell metacharacters must be shell-quoted."""
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    main(["queue", "add", "gremlins", "launch", "--plan", "#1"])
    pending = list((core.queue_root() / "pending").glob("*.cmd"))
    assert len(pending) == 1
    assert pending[0].read_text() == "gremlins launch --plan '#1'"


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


# ---------------------------------------------------------------------------
# queue list --watch
# ---------------------------------------------------------------------------


def test_cli_queue_list_watch_default_interval(tmp_path, monkeypatch):
    """--watch with no value uses interval=2 and calls render at least once."""
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    renders = []

    def fake_watch_render(interval, render):
        renders.append(interval)
        render()
        return 0

    monkeypatch.setattr("gremlins.cli.queue.watch_render", fake_watch_render)
    rc = main(["queue", "list", "--watch"])
    assert rc == 0
    assert renders == [2]


def test_cli_queue_list_watch_custom_interval(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    renders = []

    def fake_watch_render(interval, render):
        renders.append(interval)
        return 0

    monkeypatch.setattr("gremlins.cli.queue.watch_render", fake_watch_render)
    rc = main(["queue", "list", "--watch", "5"])
    assert rc == 0
    assert renders == [5]


def test_cli_queue_list_no_watch_skips_watch_render(tmp_path, monkeypatch, capsys):
    """No --watch flag: single render, watch_render not called."""
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    called = []
    monkeypatch.setattr(
        "gremlins.cli.queue.watch_render", lambda *a, **kw: called.append(1)
    )
    rc = main(["queue", "list"])
    assert rc == 0
    assert called == []
    assert "(queue is empty)" in capsys.readouterr().out


# ---------------------------------------------------------------------------
# queue list --json
# ---------------------------------------------------------------------------


def test_list_queue_json_empty(q, capsys):
    rc = core.list_queue_json()
    assert rc == 0
    out = capsys.readouterr().out
    assert json.loads(out) == []


def test_list_queue_json_shape(q, capsys):
    (q / "pending" / "0001-gh-terse.cmd").write_text(
        "gremlins launch gh-terse --description 'do the thing'"
    )
    (q / "done" / "0002-local.gr-abc123.cmd").write_text("gremlins launch local")
    rc = core.list_queue_json()
    assert rc == 0
    data = json.loads(capsys.readouterr().out)
    assert isinstance(data, list)
    assert len(data) == 2
    stems = {d["stem"] for d in data}
    assert "0001-gh-terse" in stems
    assert "0002-local.gr-abc123" in stems
    done_item = next(d for d in data if d["stem"] == "0002-local.gr-abc123")
    assert done_item["bucket"] == "done"
    assert done_item["gremlin_id"] == "gr-abc123"
    pending_item = next(d for d in data if d["stem"] == "0001-gh-terse")
    assert pending_item["bucket"] == "pending"
    assert pending_item["description"] == "do the thing"
    assert pending_item["gremlin_id"] is None


def test_list_queue_json_all_fields_present(q, capsys):
    (q / "pending" / "0001-echo.cmd").write_text("echo hello")
    core.list_queue_json()
    data = json.loads(capsys.readouterr().out)
    item = data[0]
    for field in ("bucket", "stem", "gremlin_id", "description", "cmd"):
        assert field in item


def test_cli_queue_list_json_flag(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    root = core.queue_root()
    (root / "pending" / "0001-item.cmd").write_text("echo hi")
    rc = main(["queue", "list", "--json"])
    assert rc == 0
    data = json.loads(capsys.readouterr().out)
    assert isinstance(data, list)
    assert data[0]["bucket"] == "pending"


def test_cli_queue_list_json_watch_mutually_exclusive(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    rc = main(["queue", "list", "--json", "--watch"])
    assert rc == 1
    assert "json" in capsys.readouterr().err.lower()


# ---------------------------------------------------------------------------
# set_state
# ---------------------------------------------------------------------------


def test_set_state_moves_cmd_to_target(q):
    (q / "failed" / "0001-item.cmd").write_text("echo x")
    rc = core.set_state("0001-item", "pending")
    assert rc == 0
    assert (q / "pending" / "0001-item.cmd").exists()
    assert not (q / "failed" / "0001-item.cmd").exists()


def test_set_state_moves_log_sidecar(q):
    (q / "failed" / "0001-item.cmd").write_text("echo x")
    (q / "failed" / "0001-item.log").write_text("log output")
    core.set_state("0001-item", "running")
    assert (q / "running" / "0001-item.log").exists()
    assert not (q / "failed" / "0001-item.log").exists()


def test_set_state_unknown_stem_returns_nonzero(q, capsys):
    rc = core.set_state("9999-ghost", "pending")
    assert rc == 1
    assert "9999-ghost" in capsys.readouterr().err


def test_set_state_same_state_returns_nonzero(q, capsys):
    (q / "pending" / "0001-item.cmd").write_text("echo x")
    rc = core.set_state("0001-item", "pending")
    assert rc == 1
    assert "already" in capsys.readouterr().err


def test_set_state_multi_match_returns_nonzero(q, capsys):
    (q / "pending" / "0001-item.cmd").write_text("echo x")
    (q / "failed" / "0001-item.cmd").write_text("echo x")
    rc = core.set_state("0001-item", "done")
    assert rc == 1
    err = capsys.readouterr().err
    assert "pending" in err and "failed" in err


@pytest.mark.parametrize("state", ["pending", "running", "done", "failed"])
def test_set_state_all_four_destinations(q, state):
    src = "pending" if state != "pending" else "failed"
    (q / src / "0001-item.cmd").write_text("echo x")
    rc = core.set_state("0001-item", state)
    assert rc == 0
    assert (q / state / "0001-item.cmd").exists()


def test_cli_queue_set_state_dispatches(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    root = core.queue_root()
    (root / "failed" / "0001-item.cmd").write_text("echo x")
    rc = main(["queue", "set-state", "pending", "--item", "0001-item"])
    assert rc == 0
    assert (root / "pending" / "0001-item.cmd").exists()


def test_cli_queue_set_state_invalid_state(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    with pytest.raises(SystemExit):
        main(["queue", "set-state", "bogus", "--item", "0001-item"])
