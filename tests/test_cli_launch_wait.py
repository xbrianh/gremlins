"""Tests for gremlins launch --wait."""

from __future__ import annotations

import pytest

from gremlins.cli.launch import _self_background_main


@pytest.fixture
def args(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")

    import argparse

    ns = argparse.Namespace(
        client=None,
        description=None,
        parent_id=None,
        base_ref=None,
        gremlin_id=None,
        print_id=False,
        print_id_only=False,
        wait=False,
    )
    return ns


def _fake_launch(gremlin_id: str, monkeypatch):
    monkeypatch.setattr(
        "gremlins.cli.launch.launch",
        lambda name, **kw: gremlin_id,
    )


def test_wait_exits_zero_on_clean_termination(args, monkeypatch, tmp_path):
    gremlin_id = "gr-wait01"
    _fake_launch(gremlin_id, monkeypatch)

    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True)
    import json

    (state_dir / "state.json").write_text(
        json.dumps({"status": "done", "exit_code": 0})
    )

    args.wait = True
    rc = _self_background_main("local", args, {})
    assert rc == 0


def test_wait_exits_nonzero_on_bail(args, monkeypatch, tmp_path):
    gremlin_id = "gr-wait02"
    _fake_launch(gremlin_id, monkeypatch)

    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True)
    import json

    (state_dir / "state.json").write_text(
        json.dumps({"status": "bailed", "exit_code": 0, "bail_class": "security"})
    )

    args.wait = True
    rc = _self_background_main("local", args, {})
    assert rc != 0


def test_wait_exits_nonzero_on_stopped(args, monkeypatch, tmp_path):
    gremlin_id = "gr-wait03"
    _fake_launch(gremlin_id, monkeypatch)

    state_dir = tmp_path / "state" / gremlin_id
    state_dir.mkdir(parents=True)
    import json

    (state_dir / "state.json").write_text(
        json.dumps({"status": "stopped", "exit_code": 1})
    )

    args.wait = True
    rc = _self_background_main("local", args, {})
    assert rc != 0


def test_wait_exits_nonzero_on_timeout(args, monkeypatch, tmp_path):
    gremlin_id = "gr-wait04"
    _fake_launch(gremlin_id, monkeypatch)

    monkeypatch.setattr(
        "gremlins.cli.launch._poll_terminal",
        lambda sf: (_ for _ in ()).throw(TimeoutError("timed out")),
    )

    args.wait = True
    rc = _self_background_main("local", args, {})
    assert rc != 0


def test_no_wait_exits_immediately(args, monkeypatch, tmp_path):
    gremlin_id = "gr-nowait1"
    _fake_launch(gremlin_id, monkeypatch)

    polled = []
    monkeypatch.setattr(
        "gremlins.cli.launch._poll_terminal",
        lambda sf: polled.append(sf) or {"status": "done", "exit_code": 0},
    )

    args.wait = False
    rc = _self_background_main("local", args, {})
    assert rc == 0
    assert polled == []
