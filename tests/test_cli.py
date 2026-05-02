"""Tests for gremlins/cli.py bail subcommand."""

from __future__ import annotations

import json
import pathlib

import pytest

from gremlins.cli import main


def _make_state(tmp_path: pathlib.Path, gr_id: str) -> pathlib.Path:
    """Create a minimal state.json under tmp_path/claude-gremlins/<gr_id>/state.json.

    XDG_STATE_HOME must be set to tmp_path so resolve_state_file() finds it.
    """
    state_dir = tmp_path / "claude-gremlins" / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"status": "running"}), encoding="utf-8")
    return sf


# ---------------------------------------------------------------------------
# bail subcommand — with GR_ID set
# ---------------------------------------------------------------------------

def test_bail_writes_bail_class_and_detail(tmp_path, monkeypatch):
    gr_id = "test-gremlin-001"
    sf = _make_state(tmp_path, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["bail", "other", "test reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert data["bail_class"] == "other"
    assert data["bail_detail"] == "test reason"


def test_bail_without_detail_omits_bail_detail_key(tmp_path, monkeypatch):
    gr_id = "test-gremlin-002"
    sf = _make_state(tmp_path, gr_id)
    # Pre-seed a bail_detail so we can verify it gets deleted.
    data = json.loads(sf.read_text())
    data["bail_detail"] = "stale"
    sf.write_text(json.dumps(data))

    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["bail", "secrets"])

    assert rc == 0
    result = json.loads(sf.read_text())
    assert result["bail_class"] == "secrets"
    assert "bail_detail" not in result


def test_bail_without_gr_id_exits_zero_no_write(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["bail", "other", "no gremlin context"])

    assert rc == 0
    # No claude-gremlins state directory should have been created.
    assert not (tmp_path / "claude-gremlins").exists()


def test_bail_invalid_class_exits_nonzero(tmp_path, monkeypatch):
    monkeypatch.setenv("GR_ID", "test-gremlin-003")
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    with pytest.raises(SystemExit) as exc_info:
        main(["bail", "bogus_class"])
    assert exc_info.value.code != 0


@pytest.mark.parametrize("bail_class", [
    "reviewer_requested_changes",
    "security",
    "secrets",
    "other",
])
def test_bail_all_valid_classes_accepted(tmp_path, monkeypatch, bail_class):
    gr_id = f"test-gremlin-{bail_class}"
    sf = _make_state(tmp_path, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["bail", bail_class, "reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert data["bail_class"] == bail_class
