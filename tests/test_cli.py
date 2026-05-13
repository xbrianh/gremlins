"""Tests for gremlins/cli/ dispatch."""

from __future__ import annotations

import json
import pathlib

import pytest

import gremlins.cli as cli_mod
import gremlins.executor.state as state_mod
from gremlins.cli import main
from gremlins.run_pipeline import main as run_pipeline_main
from gremlins.utils.yaml_io import YamlLoadError


@pytest.fixture
def state_root(tmp_path: pathlib.Path, monkeypatch):
    root = tmp_path / "state"
    monkeypatch.setattr("gremlins.paths.state_root", lambda: root)
    return root


def _make_state(state_root: pathlib.Path, gr_id: str) -> pathlib.Path:
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"status": "running"}), encoding="utf-8")
    return sf


# ---------------------------------------------------------------------------
# Bare invocation and removed subcommands
# ---------------------------------------------------------------------------


def test_bare_invocation_calls_fleet_status(tmp_path, monkeypatch):
    """gremlins (no args) delegates to fleet status, returns 0."""
    called = []
    monkeypatch.setattr(
        "gremlins.cli.fleet_main", lambda argv: called.append(argv) or 0
    )
    rc = main([])
    assert rc == 0
    assert called == [[]]


def test_unknown_first_arg_falls_through_to_fleet(tmp_path, monkeypatch):
    """gremlins <id-prefix> passes argv to fleet_main for drill-in."""
    received = []
    monkeypatch.setattr(
        "gremlins.cli.fleet_main", lambda argv: received.append(argv) or 0
    )
    rc = main(["abc123"])
    assert rc == 0
    assert received == [["abc123"]]


# ---------------------------------------------------------------------------
# write_bail_file / check_bail
# ---------------------------------------------------------------------------


def test_write_bail_file_creates_file(state_root, monkeypatch):
    gr_id = "gr-bail-file-a"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id}))

    state_mod.StateData.load(gr_id).write_bail_file(
        "other", "reason", attempt="test-attempt"
    )

    bail_path = state_dir / "bail_test-attempt.json"
    assert bail_path.exists()
    data = json.loads(bail_path.read_text())
    assert data["class"] == "other"
    assert data["detail"] == "reason"


def test_write_bail_file_idempotent(state_root, monkeypatch):
    gr_id = "gr-bail-file-idempotent"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id}))

    state_mod.StateData.load(gr_id).write_bail_file(
        "other", "first", attempt="attempt-1"
    )
    state_mod.StateData.load(gr_id).write_bail_file(
        "security", "second", attempt="attempt-1"
    )

    data = json.loads((state_dir / "bail_attempt-1.json").read_text())
    assert data["class"] == "other"  # not overwritten


def test_write_bail_file_noop_without_attempt(state_root, monkeypatch):
    gr_id = "gr-bail-noop"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(json.dumps({"id": gr_id}))

    state_mod.StateData.load(gr_id).write_bail_file("other", "reason", attempt="")
    bail_files = list(state_dir.glob("bail_*.json"))
    assert not bail_files


def test_check_bail_detects_bail_file(state_root, monkeypatch):
    gr_id = "gr-check-bail-file"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id, "attempt": "my-attempt"}))
    (state_dir / "bail_my-attempt.json").write_text(json.dumps({"class": "other"}))

    with pytest.raises(RuntimeError, match="bailed"):
        state_mod.StateData.load(gr_id).check_bail("test")


def test_check_bail_no_bail_file(state_root, monkeypatch):
    gr_id = "gr-check-no-bail"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id, "attempt": "my-attempt"}))
    state_mod.StateData.load(gr_id).check_bail("test")  # should not raise


def test_check_bail_stale_attempt_not_detected(state_root, monkeypatch):
    gr_id = "gr-stale-bail"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id, "attempt": "current-attempt"}))
    (state_dir / "bail_old-attempt.json").write_text(json.dumps({"class": "other"}))
    state_mod.StateData.load(gr_id).check_bail("test")  # stale bail should not raise


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
    rc = run_pipeline_main([bad_id, "/fake/pipeline.yaml"])

    assert rc != 0
    assert _no_state_created(tmp_path)


def test_run_pipeline_valid_id_proceeds(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.executor.run.run_pipeline", lambda *a, **kw: 0)
    monkeypatch.setattr(
        "gremlins.executor.state.StateData.write_terminal_state",
        lambda self, exit_code: None,
    )
    with pytest.raises(SystemExit):
        run_pipeline_main(["valid-gremlin-abc123", "/fake/pipeline.yaml"])


def test_run_pipeline_forwards_gr_id_to_orchestrator(
    tmp_path, monkeypatch, make_state_dir
):
    gr_id = "test-pipeline-gr"
    state_dir = make_state_dir(gr_id)

    from gremlins.executor.state import StateData

    def fake_run_pipeline(pipeline_path, *, argv, gr_id=None, client=None):
        StateData.load(gr_id).set_stage("implement")
        return 0

    monkeypatch.setattr("gremlins.executor.run.run_pipeline", fake_run_pipeline)
    monkeypatch.setattr(
        "gremlins.executor.state.StateData.write_terminal_state",
        lambda self, exit_code: None,
    )

    with pytest.raises(SystemExit) as exc_info:
        run_pipeline_main([gr_id, "/fake/pipeline.yaml"])
    assert exc_info.value.code == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "implement"


# ---------------------------------------------------------------------------
# Top-level fleet ops
# ---------------------------------------------------------------------------


def test_stop_dispatches_to_stop_main(tmp_path, monkeypatch):
    called = []
    monkeypatch.setitem(
        cli_mod._DISPATCH, "stop", ("", lambda argv: called.append(argv) or 0)
    )
    rc = main(["stop", "abc123"])
    assert rc == 0
    assert called == [["abc123"]]


def test_rescue_dispatches_to_rescue_main(tmp_path, monkeypatch):
    called = []
    monkeypatch.setitem(
        cli_mod._DISPATCH, "rescue", ("", lambda argv: called.append(argv) or 0)
    )
    rc = main(["rescue", "--headless", "abc123"])
    assert rc == 0
    assert called == [["--headless", "abc123"]]


def test_land_dispatches_to_land_main(tmp_path, monkeypatch):
    called = []
    monkeypatch.setitem(
        cli_mod._DISPATCH, "land", ("", lambda argv: called.append(argv) or 0)
    )
    rc = main(["land", "abc123"])
    assert rc == 0
    assert called == [["abc123"]]


# ---------------------------------------------------------------------------
# launch subcommand dispatch
# ---------------------------------------------------------------------------


def _no_state_created(tmp_path: pathlib.Path) -> bool:
    return not (tmp_path / "state").exists()


def test_launch_bare_prints_help_exits_nonzero(capsys):
    rc = main(["launch"])
    assert rc != 0
    assert "gremlins launch" in capsys.readouterr().out


def test_launch_help_flag_prints_help_exits_zero(capsys):
    rc = main(["launch", "--help"])
    assert rc == 0
    assert "gremlins launch" in capsys.readouterr().out


def test_launch_short_help_flag_prints_help_exits_zero(capsys):
    rc = main(["launch", "-h"])
    assert rc == 0
    assert "gremlins launch" in capsys.readouterr().out


def test_launch_unknown_kind_exits_nonzero_with_error(monkeypatch, capsys):
    def _raise(name, root):
        raise FileNotFoundError(f"pipeline {name!r} not found")

    monkeypatch.setattr("gremlins.cli.launch.resolve_pipeline_name", _raise)
    rc = main(["launch", "bogus"])
    assert rc != 0
    assert "bogus" in capsys.readouterr().err


# ---------------------------------------------------------------------------
# Unified dynamic dispatch
# ---------------------------------------------------------------------------


class _FakePlanStage:
    def __init__(self, entry, model, *, instructions: str) -> None:
        pass

    @classmethod
    def orchestration_args(cls):
        from gremlins.stages.base import StageInput

        return [
            StageInput(
                "instructions", str, required=False, default="", help="instructions"
            )
        ]


def _make_fake_pipeline(stage_type: str = "plan"):
    from gremlins.pipeline import Pipeline
    from gremlins.stages.plan import Plan

    stage = Plan("plan", None, [], {})
    stage.type = stage_type
    return Pipeline(name="local", path=pathlib.Path("/fake/local.yaml"), stages=[stage])


def test_launch_unified_dispatch_calls_launch(tmp_path, monkeypatch):
    monkeypatch.setattr(
        "gremlins.cli.launch.resolve_pipeline_name",
        lambda name, root: pathlib.Path(f"/fake/{name}.yaml"),
    )
    monkeypatch.setattr(
        "gremlins.cli.launch.Pipeline.from_yaml", lambda path: _make_fake_pipeline()
    )
    monkeypatch.setattr("gremlins.cli.launch.STAGE_TYPES", {"plan": _FakePlanStage})
    launched = []
    monkeypatch.setattr(
        "gremlins.cli.launch.launch",
        lambda kind, **kw: launched.append((kind, kw)) or "gr-abc123",
    )
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")

    rc = main(["launch", "local", "--instructions", "fix the bug"])
    assert rc == 0
    assert len(launched) == 1
    kind, kw = launched[0]
    assert kind == "local"
    assert kw["stage_inputs"].get("instructions") == "fix the bug"


def test_launch_unified_dispatch_no_name_exits_nonzero(capsys):
    rc = main(["launch"])
    assert rc != 0
    assert "gremlins launch" in capsys.readouterr().out


def test_launch_unified_dispatch_help_no_name_exits_zero(capsys):
    rc = main(["launch", "--help"])
    assert rc == 0
    assert "gremlins launch" in capsys.readouterr().out


def test_launch_unified_dispatch_unknown_name_exits_nonzero(monkeypatch, capsys):
    def _raise(name, root):
        raise FileNotFoundError(f"pipeline {name!r} not found; available: local, gh")

    monkeypatch.setattr("gremlins.cli.launch.resolve_pipeline_name", _raise)
    rc = main(["launch", "bogus"])
    assert rc != 0
    assert "bogus" in capsys.readouterr().err


def test_launch_unified_dispatch_unknown_name_with_help_exits_nonzero(
    monkeypatch, capsys
):
    def _raise(name, root):
        raise FileNotFoundError(f"pipeline {name!r} not found; available: local, gh")

    monkeypatch.setattr("gremlins.cli.launch.resolve_pipeline_name", _raise)
    rc = main(["launch", "bogus", "--help"])
    assert rc != 0
    assert "bogus" in capsys.readouterr().err


@pytest.mark.parametrize(
    "exc",
    [
        ValueError("duplicate stage name: 'verify'"),
        YamlLoadError("mapping values are not allowed here"),
        FileNotFoundError("prompt file not found: gremlins:missing.md"),
    ],
)
def test_launch_invalid_pipeline_exits_nonzero_with_message(
    tmp_path, monkeypatch, capsys, exc
):
    monkeypatch.setattr(
        "gremlins.cli.launch.resolve_pipeline_name",
        lambda name, root: pathlib.Path(f"/fake/{name}.yaml"),
    )

    def _raise(_path):
        raise exc

    monkeypatch.setattr("gremlins.cli.launch.Pipeline.from_yaml", _raise)
    launched = []
    monkeypatch.setattr(
        "gremlins.cli.launch.launch", lambda *a, **kw: launched.append(1) or "gr-x"
    )
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")

    rc = main(["launch", "my-pipeline"])

    assert rc == 1
    err = capsys.readouterr().err
    assert "my-pipeline" in err
    assert "invalid" in err
    assert not launched


def test_launch_unified_dispatch_help_for_resolved_pipeline(monkeypatch, capsys):
    monkeypatch.setattr(
        "gremlins.cli.launch.resolve_pipeline_name",
        lambda name, root: pathlib.Path(f"/fake/{name}.yaml"),
    )
    monkeypatch.setattr(
        "gremlins.cli.launch.Pipeline.from_yaml", lambda path: _make_fake_pipeline()
    )
    monkeypatch.setattr("gremlins.cli.launch.STAGE_TYPES", {"plan": _FakePlanStage})
    rc = main(["launch", "local", "--help"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "--instructions" in out


# ---------------------------------------------------------------------------
# launch --list
# ---------------------------------------------------------------------------


def test_launch_list_prints_pipeline_names(tmp_path, monkeypatch, capsys):
    fake_pipelines = [
        ("local", tmp_path / "local.yaml"),
        ("gh", tmp_path / "gh.yaml"),
    ]
    monkeypatch.setattr(
        "gremlins.cli.launch.list_pipelines", lambda root: fake_pipelines
    )

    from gremlins.pipeline import Pipeline

    def fake_load(path):
        return Pipeline(name=path.stem, path=path, stages=[])

    monkeypatch.setattr("gremlins.cli.launch.Pipeline.from_yaml", fake_load)

    rc = main(["launch", "--list"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "local" in out
    assert "gh" in out
    assert str(tmp_path) in out


def test_launch_list_shows_unloadable_on_exception(tmp_path, monkeypatch, capsys):
    fake_pipelines = [("broken", tmp_path / "broken.yaml")]
    monkeypatch.setattr(
        "gremlins.cli.launch.list_pipelines", lambda root: fake_pipelines
    )

    def _raise(_path):
        raise ValueError("bad yaml")

    monkeypatch.setattr("gremlins.cli.launch.Pipeline.from_yaml", _raise)

    rc = main(["launch", "--list"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "broken" in out
    assert "unloadable" in out


def test_launch_no_name_brief_mentions_list_flag(capsys):
    rc = main(["launch"])
    assert rc != 0
    out = capsys.readouterr().out
    assert "--list" in out
    assert "local|gh|boss" not in out


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
    rc = main(["resume", bad_id])
    assert rc != 0
