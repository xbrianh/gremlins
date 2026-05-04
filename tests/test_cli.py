"""Tests for gremlins/cli.py dispatch."""

from __future__ import annotations

import argparse
import json
import pathlib

import pytest

from gremlins.bail import bail_main
from gremlins.cli import (
    _validate_boss_args,
    _validate_gh_args,
    _validate_local_args,
    main,
)
from gremlins.run_pipeline import main as run_pipeline_main


def _make_state(tmp_path: pathlib.Path, gr_id: str) -> pathlib.Path:
    state_dir = tmp_path / "claude-gremlins" / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"status": "running"}), encoding="utf-8")
    return sf


# ---------------------------------------------------------------------------
# Bare invocation and removed subcommands
# ---------------------------------------------------------------------------


def test_bare_invocation_calls_fleet_status(tmp_path, monkeypatch):
    """gremlins (no args) delegates to fleet status, returns 0."""
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    called = []
    monkeypatch.setattr(
        "gremlins.cli.fleet_main", lambda argv: called.append(argv) or 0
    )
    rc = main([])
    assert rc == 0
    assert called == [[]]


def test_unknown_first_arg_falls_through_to_fleet(tmp_path, monkeypatch):
    """gremlins <id-prefix> passes argv to fleet_main for drill-in."""
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    received = []
    monkeypatch.setattr(
        "gremlins.cli.fleet_main", lambda argv: received.append(argv) or 0
    )
    rc = main(["abc123"])
    assert rc == 0
    assert received == [["abc123"]]


@pytest.mark.parametrize(
    "sub", ["fleet", "handoff", "bail", "session-summary", "_run-pipeline"]
)
def test_removed_subcommands_exit_nonzero(sub):
    rc = main([sub])
    assert rc != 0


@pytest.mark.parametrize("sub", ["local", "gh", "boss"])
def test_migrated_subcommands_exit_nonzero_with_hint(sub, capsys):
    rc = main([sub])
    assert rc != 0
    assert f"gremlins launch {sub}" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# bail_main — extracted to gremlins/bail.py
# ---------------------------------------------------------------------------


def test_bail_writes_bail_class_and_detail(tmp_path, monkeypatch):
    gr_id = "test-gremlin-001"
    sf = _make_state(tmp_path, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = bail_main(["other", "test reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert data["bail_class"] == "other"
    assert data["bail_detail"] == "test reason"


def test_bail_without_detail_omits_bail_detail_key(tmp_path, monkeypatch):
    gr_id = "test-gremlin-002"
    sf = _make_state(tmp_path, gr_id)
    data = json.loads(sf.read_text())
    data["bail_detail"] = "stale"
    sf.write_text(json.dumps(data))

    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = bail_main(["secrets"])

    assert rc == 0
    result = json.loads(sf.read_text())
    assert result["bail_class"] == "secrets"
    assert "bail_detail" not in result


def test_bail_without_gr_id_exits_zero_no_write(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = bail_main(["other", "no gremlin context"])

    assert rc == 0
    assert not (tmp_path / "claude-gremlins").exists()


def test_bail_invalid_class_exits_nonzero(tmp_path, monkeypatch):
    monkeypatch.setenv("GR_ID", "test-gremlin-003")
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    with pytest.raises(SystemExit) as exc_info:
        bail_main(["bogus_class"])
    assert exc_info.value.code != 0


@pytest.mark.parametrize(
    "bail_class",
    ["reviewer_requested_changes", "security", "secrets", "other"],
)
def test_bail_all_valid_classes_accepted(tmp_path, monkeypatch, bail_class):
    gr_id = f"test-gremlin-{bail_class}"
    sf = _make_state(tmp_path, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))

    rc = bail_main([bail_class, "reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert data["bail_class"] == bail_class


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

    rc = bail_main(["other", "reason"])

    assert rc != 0


# ---------------------------------------------------------------------------
# run_pipeline_main — extracted to gremlins/run_pipeline.py
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

    rc = run_pipeline_main([bad_id, "_local"])

    assert rc != 0
    assert not (tmp_path / "claude-gremlins").exists()


def test_run_pipeline_valid_id_proceeds(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    monkeypatch.setattr("gremlins.cli.local_main", lambda *a, **kw: 0)

    with pytest.raises(SystemExit):
        run_pipeline_main(["valid-gremlin-abc123", "_local"])


def test_run_pipeline_forwards_gr_id_to_orchestrator(
    tmp_path, monkeypatch, make_state_dir
):
    gr_id = "test-pipeline-gr"
    state_dir = make_state_dir(gr_id)

    from gremlins.state import set_stage

    def fake_local_main(argv, *, client=None, gr_id=None):
        set_stage(gr_id, "implement")
        return 0

    monkeypatch.setattr("gremlins.cli.local_main", fake_local_main)
    monkeypatch.setattr(
        "gremlins.run_pipeline.write_terminal_state", lambda gr_id, exit_code: None
    )

    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\n")

    with pytest.raises(SystemExit) as exc_info:
        run_pipeline_main([gr_id, "_local", "--plan", str(plan_file)])
    assert exc_info.value.code == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "implement"


# ---------------------------------------------------------------------------
# Top-level fleet ops
# ---------------------------------------------------------------------------


def test_stop_dispatches_to_stop_main(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    called = []
    monkeypatch.setattr("gremlins.cli.stop_main", lambda argv: called.append(argv) or 0)
    rc = main(["stop", "abc123"])
    assert rc == 0
    assert called == [["abc123"]]


def test_rescue_dispatches_to_rescue_main(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    called = []
    monkeypatch.setattr(
        "gremlins.cli.rescue_main", lambda argv: called.append(argv) or 0
    )
    rc = main(["rescue", "--headless", "abc123"])
    assert rc == 0
    assert called == [["--headless", "abc123"]]


def test_land_dispatches_to_land_main(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    called = []
    monkeypatch.setattr("gremlins.cli.land_main", lambda argv: called.append(argv) or 0)
    rc = main(["land", "abc123"])
    assert rc == 0
    assert called == [["abc123"]]


# ---------------------------------------------------------------------------
# launch subcommand dispatch
# ---------------------------------------------------------------------------


def test_launch_bare_prints_help_exits_nonzero(capsys):
    rc = main(["launch"])
    assert rc != 0
    assert "gremlins launch <kind>" in capsys.readouterr().out


def test_launch_help_flag_prints_help_exits_zero(capsys):
    rc = main(["launch", "--help"])
    assert rc == 0
    assert "gremlins launch <kind>" in capsys.readouterr().out


def test_launch_short_help_flag_prints_help_exits_zero(capsys):
    rc = main(["launch", "-h"])
    assert rc == 0
    assert "gremlins launch <kind>" in capsys.readouterr().out


def test_launch_unknown_kind_exits_nonzero_with_error(capsys):
    rc = main(["launch", "bogus"])
    assert rc != 0
    assert "unknown launch kind" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# Pre-launch validators — invalid invocations exit non-zero without state
# ---------------------------------------------------------------------------


def _no_state_created(tmp_path: pathlib.Path) -> bool:
    return not (tmp_path / "claude-gremlins").exists()


def test_local_no_args_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    rc = main(["launch", "local"])
    assert rc != 0
    assert _no_state_created(tmp_path)


def test_gh_invalid_model_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    rc = main(["launch", "gh", "--model", "!!!", "-c", "fix bug"])
    assert rc != 0
    assert _no_state_created(tmp_path)


def test_gh_bare_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    rc = main(["launch", "gh"])
    assert rc != 0
    assert _no_state_created(tmp_path)


def test_gh_invalid_resume_from_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    rc = main(["launch", "gh", "--resume-from", "bogus"])
    assert rc != 0
    assert _no_state_created(tmp_path)


def test_boss_missing_chain_kind_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    rc = main(["launch", "boss", "--plan", "x.md"])
    assert rc != 0
    assert _no_state_created(tmp_path)


# ---------------------------------------------------------------------------
# Pre-launch validators — valid invocations must not raise
# ---------------------------------------------------------------------------


def test_local_with_positional_instructions_passes():
    ns = argparse.Namespace(
        plan=None, instructions=None, positional_instructions="fix the bug"
    )
    _validate_local_args(ns)


def test_local_with_plan_passes():
    ns = argparse.Namespace(
        plan="plan.md", instructions=None, positional_instructions=None
    )
    _validate_local_args(ns)


def test_local_with_instructions_flag_passes():
    ns = argparse.Namespace(
        plan=None, instructions="fix the bug", positional_instructions=None
    )
    _validate_local_args(ns)


def test_gh_valid_model_passes():
    ns = argparse.Namespace(plan=None, instructions="fix bug")
    _validate_gh_args(ns, ["--model", "claude-sonnet-4"])


def test_gh_valid_resume_from_passes():
    ns = argparse.Namespace(plan=None, instructions=None)
    _validate_gh_args(ns, ["--resume-from", "plan"])


def test_gh_positional_instructions_passes():
    ns = argparse.Namespace(plan=None, instructions=None)
    _validate_gh_args(ns, ["fix the bug"])


def test_boss_valid_chain_kind_passes():
    _validate_boss_args(["--chain-kind", "local"], "plan.md")


def test_boss_missing_plan_raises():
    with pytest.raises(ValueError, match="--plan is required"):
        _validate_boss_args(["--chain-kind", "local"], None)


def test_boss_missing_plan_exits_nonzero_no_state(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))
    rc = main(["launch", "boss", "--chain-kind", "local"])
    assert rc != 0
    assert _no_state_created(tmp_path)


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
