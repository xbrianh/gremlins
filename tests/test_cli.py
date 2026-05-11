"""Tests for gremlins/cli.py dispatch."""

from __future__ import annotations

import json
import pathlib

import pytest
import yaml

from gremlins.bail import bail_main
from gremlins.cli import main
from gremlins.run_pipeline import main as run_pipeline_main


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
# bail_main — extracted to gremlins/bail.py
# ---------------------------------------------------------------------------


def test_bail_writes_bail_class_and_detail(state_root, monkeypatch):
    gr_id = "test-gremlin-001"
    sf = _make_state(state_root, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)

    rc = bail_main(["other", "test reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert data["bail_class"] == "other"
    assert data["bail_detail"] == "test reason"


def test_bail_without_detail_omits_bail_detail_key(state_root, monkeypatch):
    gr_id = "test-gremlin-002"
    sf = _make_state(state_root, gr_id)
    data = json.loads(sf.read_text())
    data["bail_detail"] = "stale"
    sf.write_text(json.dumps(data))

    monkeypatch.setenv("GR_ID", gr_id)

    rc = bail_main(["secrets"])

    assert rc == 0
    result = json.loads(sf.read_text())
    assert result["bail_class"] == "secrets"
    assert "bail_detail" not in result


def test_bail_with_child_key_flag_writes_parallel_shard(state_root, monkeypatch):
    gr_id = "test-gremlin-child-001"
    sf = _make_state(state_root, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)

    rc = bail_main(["--child-key", "verify-fix", "other", "test reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert "bail_class" not in data
    assert data["parallel_bails"]["verify-fix"]["bail_class"] == "other"
    assert data["parallel_bails"]["verify-fix"]["bail_detail"] == "test reason"


def test_bail_with_child_key_env_writes_parallel_shard(state_root, monkeypatch):
    gr_id = "test-gremlin-child-002"
    sf = _make_state(state_root, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("GREMLIN_CHILD_KEY", "ghreview")

    rc = bail_main(["security", "needs manual review"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert "bail_class" not in data
    assert data["parallel_bails"]["ghreview"]["bail_class"] == "security"
    assert data["parallel_bails"]["ghreview"]["bail_detail"] == "needs manual review"


def test_bail_child_key_flag_overrides_env(state_root, monkeypatch):
    gr_id = "test-gremlin-child-003"
    sf = _make_state(state_root, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setenv("GREMLIN_CHILD_KEY", "from-env")

    rc = bail_main(["--child-key", "from-flag", "other", "reason"])

    assert rc == 0
    data = json.loads(sf.read_text())
    assert "from-env" not in data.get("parallel_bails", {})
    assert data["parallel_bails"]["from-flag"]["bail_class"] == "other"


def test_bail_without_gr_id_exits_zero_no_write(tmp_path, monkeypatch):
    monkeypatch.delenv("GR_ID", raising=False)

    rc = bail_main(["other", "no gremlin context"])

    assert rc == 0
    assert _no_state_created(tmp_path)


def test_bail_invalid_class_exits_nonzero(tmp_path, monkeypatch):
    monkeypatch.setenv("GR_ID", "test-gremlin-003")

    with pytest.raises(SystemExit) as exc_info:
        bail_main(["bogus_class"])
    assert exc_info.value.code != 0


@pytest.mark.parametrize(
    "bail_class",
    ["reviewer_requested_changes", "security", "secrets", "other"],
)
def test_bail_all_valid_classes_accepted(state_root, monkeypatch, bail_class):
    gr_id = f"test-gremlin-{bail_class}"
    sf = _make_state(state_root, gr_id)
    monkeypatch.setenv("GR_ID", gr_id)

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
    rc = run_pipeline_main([bad_id, "/fake/pipeline.yaml"])

    assert rc != 0
    assert _no_state_created(tmp_path)


def test_run_pipeline_valid_id_proceeds(tmp_path, monkeypatch):
    monkeypatch.setattr("gremlins.orchestrators.run.run_pipeline", lambda *a, **kw: 0)
    monkeypatch.setattr(
        "gremlins.run_pipeline.write_terminal_state", lambda gr_id, exit_code: None
    )
    with pytest.raises(SystemExit):
        run_pipeline_main(["valid-gremlin-abc123", "/fake/pipeline.yaml"])


def test_run_pipeline_forwards_gr_id_to_orchestrator(
    tmp_path, monkeypatch, make_state_dir
):
    gr_id = "test-pipeline-gr"
    state_dir = make_state_dir(gr_id)

    from gremlins.state import set_stage

    def fake_run_pipeline(pipeline_path, *, argv, gr_id=None, client=None):
        set_stage(gr_id, "implement")
        return 0

    monkeypatch.setattr("gremlins.orchestrators.run.run_pipeline", fake_run_pipeline)
    monkeypatch.setattr(
        "gremlins.run_pipeline.write_terminal_state", lambda gr_id, exit_code: None
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
    monkeypatch.setattr("gremlins.cli.stop_main", lambda argv: called.append(argv) or 0)
    rc = main(["stop", "abc123"])
    assert rc == 0
    assert called == [["abc123"]]


def test_rescue_dispatches_to_rescue_main(tmp_path, monkeypatch):
    called = []
    monkeypatch.setattr(
        "gremlins.cli.rescue_main", lambda argv: called.append(argv) or 0
    )
    rc = main(["rescue", "--headless", "abc123"])
    assert rc == 0
    assert called == [["--headless", "abc123"]]


def test_land_dispatches_to_land_main(tmp_path, monkeypatch):
    called = []
    monkeypatch.setattr("gremlins.cli.land_main", lambda argv: called.append(argv) or 0)
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

    monkeypatch.setattr("gremlins.cli.resolve_pipeline_name", _raise)
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
        "gremlins.cli.resolve_pipeline_name",
        lambda name, root: pathlib.Path(f"/fake/{name}.yaml"),
    )
    monkeypatch.setattr(
        "gremlins.cli.Pipeline.from_yaml", lambda path: _make_fake_pipeline()
    )
    monkeypatch.setattr("gremlins.cli.STAGE_REGISTRY", {"plan": _FakePlanStage})
    launched = []
    monkeypatch.setattr(
        "gremlins.cli.launch",
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

    monkeypatch.setattr("gremlins.cli.resolve_pipeline_name", _raise)
    rc = main(["launch", "bogus"])
    assert rc != 0
    assert "bogus" in capsys.readouterr().err


def test_launch_unified_dispatch_unknown_name_with_help_exits_nonzero(
    monkeypatch, capsys
):
    def _raise(name, root):
        raise FileNotFoundError(f"pipeline {name!r} not found; available: local, gh")

    monkeypatch.setattr("gremlins.cli.resolve_pipeline_name", _raise)
    rc = main(["launch", "bogus", "--help"])
    assert rc != 0
    assert "bogus" in capsys.readouterr().err


@pytest.mark.parametrize(
    "exc",
    [
        ValueError("duplicate stage name: 'verify'"),
        yaml.YAMLError("mapping values are not allowed here"),
        FileNotFoundError("prompt file not found: gremlins:missing.md"),
    ],
)
def test_launch_invalid_pipeline_exits_nonzero_with_message(
    tmp_path, monkeypatch, capsys, exc
):
    monkeypatch.setattr(
        "gremlins.cli.resolve_pipeline_name",
        lambda name, root: pathlib.Path(f"/fake/{name}.yaml"),
    )

    def _raise(_path):
        raise exc

    monkeypatch.setattr("gremlins.cli.Pipeline.from_yaml", _raise)
    launched = []
    monkeypatch.setattr(
        "gremlins.cli.launch", lambda *a, **kw: launched.append(1) or "gr-x"
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
        "gremlins.cli.resolve_pipeline_name",
        lambda name, root: pathlib.Path(f"/fake/{name}.yaml"),
    )
    monkeypatch.setattr(
        "gremlins.cli.Pipeline.from_yaml", lambda path: _make_fake_pipeline()
    )
    monkeypatch.setattr("gremlins.cli.STAGE_REGISTRY", {"plan": _FakePlanStage})
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
    monkeypatch.setattr("gremlins.cli.list_pipelines", lambda root: fake_pipelines)

    from gremlins.pipeline import Pipeline

    def fake_load(path):
        return Pipeline(name=path.stem, path=path, stages=[])

    monkeypatch.setattr("gremlins.cli.Pipeline.from_yaml", fake_load)

    rc = main(["launch", "--list"])
    assert rc == 0
    out = capsys.readouterr().out
    assert "local" in out
    assert "gh" in out
    assert str(tmp_path) in out


def test_launch_list_shows_unloadable_on_exception(tmp_path, monkeypatch, capsys):
    fake_pipelines = [("broken", tmp_path / "broken.yaml")]
    monkeypatch.setattr("gremlins.cli.list_pipelines", lambda root: fake_pipelines)

    def _raise(_path):
        raise ValueError("bad yaml")

    monkeypatch.setattr("gremlins.cli.Pipeline.from_yaml", _raise)

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
