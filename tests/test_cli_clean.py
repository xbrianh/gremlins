import json
import os
from pathlib import Path

import pytest

from gremlins.cli.clean import _scan_state, clean_main


@pytest.mark.usefixtures("sandbox")
def test_clean_state_skips_live_gremlin(capsys):
    gid = "live123"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(
        json.dumps({"status": "running", "pid": os.getpid()})
    )
    (sd / "log").touch()
    items = _scan_state()
    assert items == []
    captured = capsys.readouterr()
    assert "nothing to clean" not in captured.out


@pytest.mark.usefixtures("sandbox")
def test_clean_state_collects_dead_gremlin(capsys):
    gid = "dead123"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(json.dumps({"pid": 999999}))
    (sd / "finished").touch()
    items = _scan_state()
    assert len(items) == 1
    assert items[0].label == gid


@pytest.mark.usefixtures("sandbox")
def test_clean_state_failed_filter_excludes_succeeded(capsys):
    root = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state"
    for gid, ec in [("fail1", 1), ("succ1", 0)]:
        sd = root / gid
        sd.mkdir()
        (sd / "state.json").write_text(json.dumps({"exit_code": ec}))
        (sd / "finished").touch()
    items = _scan_state(failed=True)
    assert len(items) == 1
    assert items[0].label == "fail1"


@pytest.mark.usefixtures("sandbox")
def test_clean_state_failed_filter_includes_failed(capsys):
    root = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state"
    for gid, ec in [("fail1", 1), ("succ1", 0)]:
        sd = root / gid
        sd.mkdir()
        (sd / "state.json").write_text(json.dumps({"exit_code": ec}))
        (sd / "finished").touch()
    items = _scan_state(failed=True)
    assert any(i.label == "fail1" for i in items)


@pytest.mark.usefixtures("sandbox")
def test_clean_state_skips_parallel_child_dirs(capsys):
    gid = "abc123--parallel--child"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(json.dumps({"pid": 999999}))
    (sd / "finished").touch()
    items = _scan_state()
    assert items == []


@pytest.mark.usefixtures("sandbox")
def test_clean_state_parallel_children_collected_with_parent(capsys):
    root = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state"
    parent = root / "abc123"
    parent.mkdir()
    (parent / "state.json").write_text(json.dumps({"pid": 999999}))
    (parent / "finished").touch()
    child = root / "abc123--parallel--child"
    child.mkdir()
    (child / "state.json").write_text(json.dumps({"pid": 999999}))
    (child / "finished").touch()
    items = _scan_state()
    labels = {i.label for i in items}
    assert labels == {"abc123", "abc123--parallel--child"}


@pytest.mark.usefixtures("sandbox")
def test_clean_state_delete_removes_state_dir(capsys):
    gid = "todel123"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(json.dumps({"pid": 999999}))
    (sd / "finished").touch()
    ret = clean_main(["--state", "--yes"])
    assert ret == 0
    assert not sd.exists()


@pytest.mark.usefixtures("sandbox")
def test_clean_no_flags_prints_summary_no_deletion(capsys):
    gid = "sum123"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(json.dumps({"pid": 999999}))
    (sd / "finished").touch()
    ret = clean_main([])
    assert ret == 0
    out = capsys.readouterr().out
    assert "state:" in out
    assert sd.exists()


@pytest.mark.usefixtures("sandbox")
def test_clean_dry_run_leaves_everything(capsys):
    gid = "dry123"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(json.dumps({"pid": 999999}))
    (sd / "finished").touch()
    ret = clean_main(["--state", "--dry-run"])
    assert ret == 0
    assert sd.exists()


@pytest.mark.usefixtures("sandbox")
def test_clean_yes_skips_prompt(capsys):
    gid = "yes123"
    sd = Path(os.environ["GREMLINS_SANDBOX_ROOT"]) / "state" / gid
    sd.mkdir(parents=True)
    (sd / "state.json").write_text(json.dumps({"pid": 999999}))
    (sd / "finished").touch()
    ret = clean_main(["--state", "--yes"])
    assert ret == 0
    assert not sd.exists()


@pytest.mark.usefixtures("sandbox")
def test_clean_nothing_to_clean_message(capsys):
    ret = clean_main(["--state", "--yes"])
    assert ret == 0
    out = capsys.readouterr().out
    assert "nothing to clean" in out
