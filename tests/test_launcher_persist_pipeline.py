"""Tests for pipeline.yaml hermetic persistence at launch time."""

from __future__ import annotations

import json
import pathlib
import secrets

import yaml

from gremlins.cli.launch import launch_main
from gremlins.pipeline import Pipeline


def _new_gremlin_id() -> str:
    return secrets.token_hex(8)


# ---------------------------------------------------------------------------
# persist_expanded_pipeline — unit tests
# ---------------------------------------------------------------------------


def test_persist_expanded_pipeline_writes_file(tmp_path):
    """persist_expanded_pipeline writes a valid YAML file to the state dir."""
    from gremlins.launcher import persist_expanded_pipeline

    pipeline_yaml = tmp_path / "proj.yaml"
    pipeline_yaml.write_text(
        """\
prompts:
  impl-prompt: |
    Do the implementation.
stages:
  - name: plan
    type: agent
  - name: implement
    type: implement
    prompt: impl-prompt
""",
        encoding="utf-8",
    )

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    dest = persist_expanded_pipeline(state_dir, str(pipeline_yaml))

    assert dest == str(state_dir / "pipeline.yaml")
    assert (state_dir / "pipeline.yaml").is_file()

    parsed = yaml.safe_load((state_dir / "pipeline.yaml").read_text(encoding="utf-8"))
    assert isinstance(parsed, dict)
    stage_names = [s["name"] for s in parsed["stages"]]
    # implement recipe expands to implement + git-commit + require-impl-progress
    assert stage_names == ["plan", "implement", "git-commit", "require-impl-progress"]


def test_persist_expanded_pipeline_roundtrips_via_pipeline_from_yaml(tmp_path):
    """Pipeline.from_yaml on the persisted copy produces the same stages as the original."""
    from gremlins.launcher import persist_expanded_pipeline

    pipeline_yaml = tmp_path / "proj.yaml"
    pipeline_yaml.write_text(
        """\
stages:
  - name: plan
    type: agent
  - name: implement
    type: agent
    prompt: []
""",
        encoding="utf-8",
    )

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    dest = persist_expanded_pipeline(state_dir, str(pipeline_yaml))

    original = Pipeline.from_yaml(pipeline_yaml)
    persisted = Pipeline.from_yaml(pathlib.Path(dest))

    assert [s.name for s in original.stages] == [s.name for s in persisted.stages]
    assert [s.type for s in original.stages] == [s.type for s in persisted.stages]


# ---------------------------------------------------------------------------
# launch() — state reflects hermetic path
# ---------------------------------------------------------------------------


def test_launch_pipeline_path_points_to_state_dir(lenv):
    """After launch, state.pipeline_path points to <state_dir>/pipeline.yaml."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "persist pipeline test", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state_dir = lenv.state_root / gremlin_id
    state = json.loads((state_dir / "state.json").read_text(encoding="utf-8"))

    expected = str(state_dir / "pipeline.yaml")
    assert state["pipeline_path"] == expected
    assert (state_dir / "pipeline.yaml").is_file()


def test_launch_persisted_pipeline_is_valid_yaml(lenv):
    """<state_dir>/pipeline.yaml written at launch is a valid YAML mapping with stages."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "yaml validity test", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state_dir = lenv.state_root / gremlin_id
    text = (state_dir / "pipeline.yaml").read_text(encoding="utf-8")
    parsed = yaml.safe_load(text)
    assert isinstance(parsed, dict)
    assert "stages" in parsed
    assert isinstance(parsed["stages"], list)
    assert len(parsed["stages"]) > 0


# ---------------------------------------------------------------------------
# resume() — uses hermetic copy
# ---------------------------------------------------------------------------


def test_resume_uses_hermetic_pipeline_yaml(lenv, monkeypatch):
    """resume() passes state_dir/pipeline.yaml as the pipeline when it exists."""
    from gremlins import launcher

    gremlin_id = "resume-hermetic-check"
    state_dir = lenv.state_root / gremlin_id
    state_dir.mkdir(parents=True)

    project_yaml = lenv.repo / ".gremlins" / "local.yaml"
    project_yaml.parent.mkdir(parents=True, exist_ok=True)
    project_yaml.write_text(
        """\
stages:
  - name: plan
    type: agent
  - name: implement
    type: implement
""",
        encoding="utf-8",
    )

    # Write the hermetic copy with a distinct stage list.
    hermetic_yaml = state_dir / "pipeline.yaml"
    hermetic_yaml.write_text(
        """\
stages:
  - name: only-stage
    type: agent
""",
        encoding="utf-8",
    )

    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "kind": "local",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "only-stage",
                "status": "stopped",
                "exit_code": 1,
                "pipeline_args": [],
                "pipeline_path": str(hermetic_yaml),
            }
        ),
        encoding="utf-8",
    )
    (state_dir / "instructions.txt").write_text("test", encoding="utf-8")

    captured: dict[str, object] = {}

    class _Proc:
        pid = 12345

    def fake_spawn(cmd, cwd, env, log_path, log_mode="w"):
        # cmd: [python, -m, gremlins.spawn.pipeline, gremlin_id, pipeline_path, *args]
        captured["pipeline_path"] = cmd[4]
        return _Proc()

    monkeypatch.setattr(launcher, "_spawn_logged_process", fake_spawn)

    launcher.resume(gremlin_id)

    assert captured["pipeline_path"] == str(hermetic_yaml)


def test_resume_hermetic_copy_isolates_from_project_yaml_edits(lenv, monkeypatch):
    """Editing the project YAML after launch does not change what resume runs."""
    from gremlins import launcher

    gremlin_id = "resume-hermetic-isolation"
    state_dir = lenv.state_root / gremlin_id
    state_dir.mkdir(parents=True)

    project_yaml = lenv.repo / ".gremlins" / "local.yaml"
    project_yaml.parent.mkdir(parents=True, exist_ok=True)
    original_content = """\
stages:
  - name: plan
    type: agent
"""
    project_yaml.write_text(original_content, encoding="utf-8")

    # Persist the hermetic copy (as launch() would have done).
    hermetic_yaml = state_dir / "pipeline.yaml"
    hermetic_yaml.write_text(original_content, encoding="utf-8")

    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "kind": "local",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "plan",
                "status": "stopped",
                "exit_code": 1,
                "pipeline_args": [],
                "pipeline_path": str(hermetic_yaml),
            }
        ),
        encoding="utf-8",
    )
    (state_dir / "instructions.txt").write_text("test", encoding="utf-8")

    # Mutate the project YAML — add an extra stage.
    project_yaml.write_text(
        """\
stages:
  - name: plan
    type: agent
  - name: extra-stage
    type: implement
""",
        encoding="utf-8",
    )

    captured: dict[str, object] = {}

    class _Proc:
        pid = 12345

    def fake_spawn(cmd, cwd, env, log_path, log_mode="w"):
        captured["pipeline_path"] = cmd[4]
        return _Proc()

    monkeypatch.setattr(launcher, "_spawn_logged_process", fake_spawn)

    launcher.resume(gremlin_id)

    # Must use the hermetic copy, not the mutated project YAML.
    assert captured["pipeline_path"] == str(hermetic_yaml)
    resumed_stages = yaml.safe_load(hermetic_yaml.read_text(encoding="utf-8"))["stages"]
    assert [s["name"] for s in resumed_stages] == ["plan"]
