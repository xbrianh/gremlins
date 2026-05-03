"""Tests for gremlins/cli.py bail, resume, and _run-pipeline subcommands."""

from __future__ import annotations

import argparse
import json
import pathlib

import pytest

from gremlins.cli import (
    _validate_boss_args,
    _validate_gh_args,
    _validate_local_args,
    main,
)


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


@pytest.mark.parametrize(
    "bail_class",
    [
        "reviewer_requested_changes",
        "security",
        "secrets",
        "other",
    ],
)
def test_bail_all_valid_classes_accepted(tmp_path, monkeypatch, bail_class):
    gr_id = f"test-gremlin-{bail_class}"
    sf = _make_state(tmp_path, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["bail", bail_class, "reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert data["bail_class"] == bail_class


# ---------------------------------------------------------------------------
# _run-pipeline subcommand — gr_id validation
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "bad_id",
    [
        "",
        "../escape",
        "foo/bar",
        "foo\\bar",
        "foo..bar",
        "id with spaces",
        "id;injection",
    ],
)
def test_run_pipeline_rejects_invalid_gr_id(tmp_path, monkeypatch, bad_id):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["_run-pipeline", bad_id, "_local"])

    assert rc != 0
    assert not (tmp_path / "claude-gremlins").exists()


def test_run_pipeline_valid_id_proceeds(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    monkeypatch.setattr("gremlins.cli.local_main", lambda *a, **kw: 0)

    with pytest.raises(SystemExit):
        main(["_run-pipeline", "valid-gremlin-abc123", "_local"])
    # If we reach here, validate_gr_id passed; pipeline may exit for any reason.


def test_run_pipeline_forwards_gr_id_to_orchestrator(
    tmp_path, monkeypatch, make_state_dir
):
    """_run-pipeline <gr_id> _local ... passes gr_id down to local_main."""
    gr_id = "test-pipeline-gr"
    state_dir = make_state_dir(gr_id)

    from gremlins.state import set_stage

    def fake_local_main(argv, *, client=None, gr_id=None):
        set_stage(gr_id, "implement")
        return 0

    monkeypatch.setattr("gremlins.cli.local_main", fake_local_main)
    monkeypatch.setattr(
        "gremlins.cli.write_terminal_state", lambda gr_id, exit_code: None
    )

    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\n")

    with pytest.raises(SystemExit) as exc_info:
        main(["_run-pipeline", gr_id, "_local", "--plan", str(plan_file)])
    assert exc_info.value.code == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "implement"


# ---------------------------------------------------------------------------
# Pre-launch validators — invalid invocations must exit non-zero without
# touching XDG_STATE_HOME.
# ---------------------------------------------------------------------------


def _no_state_created(tmp_path: pathlib.Path) -> bool:
    return not (tmp_path / "claude-gremlins").exists()


def test_local_no_args_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["local"])

    assert rc != 0
    assert _no_state_created(tmp_path)


def test_gh_invalid_model_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["gh", "--model", "!!!", "-c", "fix bug"])

    assert rc != 0
    assert _no_state_created(tmp_path)


def test_gh_bare_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["gh"])

    assert rc != 0
    assert _no_state_created(tmp_path)


def test_gh_invalid_resume_from_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["gh", "--resume-from", "bogus"])

    assert rc != 0
    assert _no_state_created(tmp_path)


def test_boss_missing_chain_kind_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["boss", "--plan", "x.md"])

    assert rc != 0
    assert _no_state_created(tmp_path)


# ---------------------------------------------------------------------------
# Pre-launch validators — valid invocations must not raise.
# ---------------------------------------------------------------------------


def test_local_with_positional_instructions_passes():
    ns = argparse.Namespace(
        plan=None, instructions=None, positional_instructions="fix the bug"
    )
    _validate_local_args(ns)  # must not raise


def test_local_with_plan_passes():
    ns = argparse.Namespace(
        plan="plan.md", instructions=None, positional_instructions=None
    )
    _validate_local_args(ns)  # must not raise


def test_local_with_instructions_flag_passes():
    ns = argparse.Namespace(
        plan=None, instructions="fix the bug", positional_instructions=None
    )
    _validate_local_args(ns)  # must not raise


def test_gh_valid_model_passes():
    ns = argparse.Namespace(plan=None, instructions="fix bug")
    _validate_gh_args(ns, ["--model", "claude-sonnet-4"])  # must not raise


def test_boss_valid_chain_kind_passes():
    _validate_boss_args(["--chain-kind", "local"])  # must not raise


# ---------------------------------------------------------------------------
# resume subcommand — gr_id validation
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "bad_id",
    [
        "",
        "../escape",
        "foo/bar",
        "foo\\bar",
        "foo..bar",
        "id with spaces",
        "id;injection",
    ],
)
def test_resume_rejects_invalid_gr_id(tmp_path, monkeypatch, bad_id):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["resume", bad_id])

    assert rc != 0


# ---------------------------------------------------------------------------
# bail subcommand — GR_ID validation
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "bad_id",
    [
        "",
        "../escape",
        "foo/bar",
        "foo\\bar",
        "foo..bar",
        "id with spaces",
        "id;injection",
    ],
)
def test_bail_rejects_malformed_gr_id_env(tmp_path, monkeypatch, bad_id):
    monkeypatch.setenv("GR_ID", bad_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = main(["bail", "other", "reason"])

    assert rc != 0
