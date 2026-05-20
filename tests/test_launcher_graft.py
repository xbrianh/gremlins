"""Tests for gremlins resume --graft."""

from __future__ import annotations

import json
import os
import pathlib

import pytest
import yaml

# ---------------------------------------------------------------------------
# _disambiguate_graft_names — unit tests
# ---------------------------------------------------------------------------


def test_disambiguate_no_collision():
    from gremlins.launcher import _disambiguate_graft_names

    stages = [{"name": "address"}, {"name": "push"}]
    _disambiguate_graft_names(stages, {"plan", "implement"})
    assert [s["name"] for s in stages] == ["address", "push"]


def test_disambiguate_collision_gets_suffix():
    from gremlins.launcher import _disambiguate_graft_names

    stages = [{"name": "plan"}]
    _disambiguate_graft_names(stages, {"plan"})
    assert stages[0]["name"] == "plan-2"


def test_disambiguate_suffix_increments_past_taken():
    from gremlins.launcher import _disambiguate_graft_names

    stages = [{"name": "plan"}]
    _disambiguate_graft_names(stages, {"plan", "plan-2"})
    assert stages[0]["name"] == "plan-3"


def test_disambiguate_multiple_collisions():
    from gremlins.launcher import _disambiguate_graft_names

    stages = [{"name": "plan"}, {"name": "plan"}]
    _disambiguate_graft_names(stages, {"plan"})
    assert stages[0]["name"] == "plan-2"
    assert stages[1]["name"] == "plan-3"


def test_disambiguate_parallel_child_collision_with_existing_top_level():
    from gremlins.launcher import _disambiguate_graft_names

    # Grafted parallel child collides with an existing top-level name.
    stages = [{"name": "review", "type": "parallel", "body": [{"name": "plan"}]}]
    _disambiguate_graft_names(stages, {"plan"})
    assert stages[0]["body"][0]["name"] == "plan-2"


def test_disambiguate_parallel_children_collide_with_each_other():
    from gremlins.launcher import _disambiguate_graft_names

    stages = [
        {
            "name": "review",
            "type": "parallel",
            "body": [{"name": "check"}, {"name": "check"}],
        }
    ]
    _disambiguate_graft_names(stages, set())
    assert stages[0]["body"][0]["name"] == "check"
    assert stages[0]["body"][1]["name"] == "check-2"


# ---------------------------------------------------------------------------
# _append_graft — unit tests
# ---------------------------------------------------------------------------


def _write_pipeline(path: pathlib.Path, stages: list) -> None:
    path.write_text(
        yaml.dump({"__gremlins_expanded__": True, "stages": stages}),
        encoding="utf-8",
    )


def test_append_graft_adds_stages_flat(tmp_path):
    from gremlins.launcher import _append_graft

    hermetic = tmp_path / "state" / "pipeline.yaml"
    hermetic.parent.mkdir()
    _write_pipeline(hermetic, [{"name": "plan", "type": "plan"}])

    graft_yaml = tmp_path / ".gremlins" / "address.yaml"
    graft_yaml.parent.mkdir()
    graft_yaml.write_text(
        yaml.dump({"stages": [{"name": "address", "type": "address"}]}),
        encoding="utf-8",
    )

    result = _append_graft(hermetic.parent, "address", str(tmp_path))

    loaded = yaml.safe_load(hermetic.read_text(encoding="utf-8"))
    top = loaded["stages"]
    assert len(top) == 2
    assert top[1]["name"] == "address"
    assert top[1]["type"] == "address"
    assert result == "address"


def test_append_graft_disambiguates_collision(tmp_path):
    from gremlins.launcher import _append_graft

    hermetic = tmp_path / "state" / "pipeline.yaml"
    hermetic.parent.mkdir()
    _write_pipeline(
        hermetic,
        [
            {"name": "plan", "type": "plan"},
            {"name": "address", "type": "address"},
        ],
    )

    graft_yaml = tmp_path / ".gremlins" / "review.yaml"
    graft_yaml.parent.mkdir()
    graft_yaml.write_text(
        yaml.dump({"stages": [{"name": "address", "type": "address"}]}),
        encoding="utf-8",
    )

    result = _append_graft(hermetic.parent, "review", str(tmp_path))

    loaded = yaml.safe_load(hermetic.read_text(encoding="utf-8"))
    assert loaded["stages"][-1]["name"] == "address-2"
    assert result == "address-2"


def test_append_graft_unnamed_first_stage_gets_type_name(tmp_path):
    from gremlins.launcher import _append_graft

    hermetic = tmp_path / "state" / "pipeline.yaml"
    hermetic.parent.mkdir()
    _write_pipeline(hermetic, [{"name": "plan", "type": "plan"}])

    graft_yaml = tmp_path / ".gremlins" / "address.yaml"
    graft_yaml.parent.mkdir()
    # No name; _fill_names should derive it from type.
    graft_yaml.write_text(
        yaml.dump({"stages": [{"type": "address"}]}),
        encoding="utf-8",
    )

    result = _append_graft(hermetic.parent, "address", str(tmp_path))

    loaded = yaml.safe_load(hermetic.read_text(encoding="utf-8"))
    assert result == "address"
    assert loaded["stages"][-1]["name"] == "address"


def test_append_graft_existing_parallel_child_blocks_graft_name(tmp_path):
    from gremlins.launcher import _append_graft

    hermetic = tmp_path / "state" / "pipeline.yaml"
    hermetic.parent.mkdir()
    # Existing pipeline has a parallel stage with child named "check".
    _write_pipeline(
        hermetic,
        [
            {
                "name": "parallel-group",
                "type": "parallel",
                "body": [{"name": "check", "type": "plan"}],
            }
        ],
    )

    graft_yaml = tmp_path / ".gremlins" / "review.yaml"
    graft_yaml.parent.mkdir()
    # Graft top-level stage collides with existing parallel child name.
    graft_yaml.write_text(
        yaml.dump({"stages": [{"name": "check", "type": "plan"}]}),
        encoding="utf-8",
    )

    result = _append_graft(hermetic.parent, "review", str(tmp_path))

    loaded = yaml.safe_load(hermetic.read_text(encoding="utf-8"))
    assert result == "check-2"
    assert loaded["stages"][-1]["name"] == "check-2"


def test_append_graft_no_hermetic_raises(tmp_path):
    from gremlins.launcher import _append_graft

    state_dir = tmp_path / "state"
    state_dir.mkdir()
    with pytest.raises(RuntimeError, match="no persisted pipeline.yaml"):
        _append_graft(state_dir, "anything", str(tmp_path))


# ---------------------------------------------------------------------------
# resume(graft=...) — integration-style tests (monkeypatched spawn)
# ---------------------------------------------------------------------------


def _make_state(state_dir: pathlib.Path, repo: pathlib.Path, **extra) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    data = {
        "id": state_dir.name,
        "kind": "localgremlin",
        "workdir": str(repo),
        "project_root": str(repo),
        "stage": "ci-gate",
        "status": "done",
        "exit_code": 0,
        **extra,
    }
    (state_dir / "state.json").write_text(json.dumps(data), encoding="utf-8")
    (state_dir / "finished").touch()
    (state_dir / "log").write_text("", encoding="utf-8")
    (state_dir / "instructions.txt").write_text("test instructions", encoding="utf-8")


def _write_hermetic(state_dir: pathlib.Path, stages: list | None = None) -> None:
    if stages is None:
        stages = [{"name": "plan", "type": "plan"}, {"name": "ci-gate", "type": "plan"}]
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "pipeline.yaml").write_text(
        yaml.dump({"__gremlins_expanded__": True, "stages": stages}),
        encoding="utf-8",
    )


def _write_graft_pipeline(repo: pathlib.Path, name: str, stage_name: str) -> None:
    gremlins_dir = repo / ".gremlins"
    gremlins_dir.mkdir(exist_ok=True)
    (gremlins_dir / f"{name}.yaml").write_text(
        yaml.dump({"stages": [{"name": stage_name, "type": "plan"}]}),
        encoding="utf-8",
    )


class _FakeProc:
    pid = 99999


def test_graft_on_running_gremlin_raises(lenv, monkeypatch):
    """resume(graft=...) must refuse if the gremlin is still running."""
    from gremlins import launcher
    from gremlins.launcher import GremlinAlreadyRunning

    gremlin_id = "graft-running-test"
    state_dir = lenv.state_root / gremlin_id
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "kind": "localgremlin",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "plan",
                "status": "running",
                "pid": os.getpid(),
                "pipeline_args": [],
            }
        ),
        encoding="utf-8",
    )
    _write_graft_pipeline(lenv.repo, "address", "address")

    with pytest.raises(GremlinAlreadyRunning, match="still running"):
        launcher.resume(gremlin_id, graft="address")


def test_graft_on_finished_success_works(lenv, monkeypatch):
    """resume(graft=...) succeeds even when gremlin finished successfully."""
    from gremlins import launcher

    gremlin_id = "graft-finished-test"
    state_dir = lenv.state_root / gremlin_id
    _make_state(state_dir, lenv.repo)
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address")

    captured: dict[str, object] = {}

    def fake_spawn(cmd, cwd, env, log_path, log_mode="w"):
        captured["cmd"] = cmd
        return _FakeProc()

    monkeypatch.setattr(launcher, "_spawn_logged_process", fake_spawn)

    launcher.resume(gremlin_id, graft="address")

    state = json.loads((state_dir / "state.json").read_text(encoding="utf-8"))
    assert state["status"] == "running"
    assert not (state_dir / "finished").exists()

    cmd = list(captured["cmd"])
    assert "--resume-from" in cmd
    assert cmd[cmd.index("--resume-from") + 1] == "address"


def test_graft_appends_flat_stages(lenv, monkeypatch):
    """resume(graft=...) appends stages flat to the hermetic pipeline.yaml."""
    from gremlins import launcher

    gremlin_id = "graft-append-test"
    state_dir = lenv.state_root / gremlin_id
    _make_state(state_dir, lenv.repo)
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address-code")

    monkeypatch.setattr(launcher, "_spawn_logged_process", lambda *a, **kw: _FakeProc())

    launcher.resume(gremlin_id, graft="address")

    pipeline = yaml.safe_load((state_dir / "pipeline.yaml").read_text(encoding="utf-8"))
    top = pipeline["stages"]
    appended = top[-1]
    assert appended["name"] == "address-code"
    assert appended["type"] == "plan"
    assert "body" not in appended


def test_repeated_grafts_produce_real_names(lenv, monkeypatch):
    """Two grafts append real stage names with no collisions."""
    from gremlins import launcher

    gremlin_id = "graft-repeat-test"
    state_dir = lenv.state_root / gremlin_id
    _make_state(state_dir, lenv.repo)
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address-code")
    _write_graft_pipeline(lenv.repo, "review", "review-code")

    monkeypatch.setattr(launcher, "_spawn_logged_process", lambda *a, **kw: _FakeProc())

    launcher.resume(gremlin_id, graft="address")
    # Manually restore finished/done so we can graft again
    (state_dir / "finished").touch()
    state = json.loads((state_dir / "state.json").read_text(encoding="utf-8"))
    state["status"] = "done"
    state["exit_code"] = 0
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")

    launcher.resume(gremlin_id, graft="review")

    pipeline = yaml.safe_load((state_dir / "pipeline.yaml").read_text(encoding="utf-8"))
    names = [s["name"] for s in pipeline["stages"]]
    assert "address-code" in names
    assert "review-code" in names


def test_resume_without_graft_after_graft_uses_updated_pipeline(lenv, monkeypatch):
    """gremlins resume (no --graft) after a graft uses the updated pipeline.yaml."""
    from gremlins import launcher

    gremlin_id = "graft-plain-resume-test"
    state_dir = lenv.state_root / gremlin_id
    _make_state(
        state_dir,
        lenv.repo,
        status="stopped",
        exit_code=1,
    )
    # Remove finished marker so plain resume works
    (state_dir / "finished").unlink(missing_ok=True)
    # Write hermetic pipeline that already has a grafted stage
    _write_hermetic(
        state_dir,
        [
            {"name": "plan", "type": "plan"},
            {"name": "address-code", "type": "plan"},
        ],
    )

    captured: dict[str, object] = {}

    def fake_spawn(cmd, cwd, env, log_path, log_mode="w"):
        captured["pipeline"] = cmd[4]
        captured["spawn_args"] = list(cmd[5:])
        return _FakeProc()

    monkeypatch.setattr(launcher, "_spawn_logged_process", fake_spawn)

    launcher.resume(gremlin_id)

    assert captured["pipeline"] == str(state_dir / "pipeline.yaml")
    spawn_args = list(captured["spawn_args"])
    assert "--resume-from" in spawn_args


def test_graft_missing_worktree_raises(lenv, monkeypatch):
    """resume(graft=...) raises immediately if the worktree directory is gone."""
    from gremlins import launcher

    gremlin_id = "graft-no-worktree-test"
    state_dir = lenv.state_root / gremlin_id
    missing_workdir = lenv.repo.parent / "nonexistent-worktree"

    _make_state(state_dir, lenv.repo, workdir=str(missing_workdir))
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address-code")

    monkeypatch.setattr(launcher, "_spawn_logged_process", lambda *a, **kw: _FakeProc())

    with pytest.raises(RuntimeError, match="worktree missing"):
        launcher.resume(gremlin_id, graft="address")

    # State must not be mutated (finished marker still present)
    assert (state_dir / "finished").exists()


def test_graft_succeeds_without_branch(lenv, monkeypatch):
    """Graft proceeds even when artifacts reference a deleted branch."""
    from gremlins import launcher

    gremlin_id = "graft-no-branch-test"
    state_dir = lenv.state_root / gremlin_id

    # worktree_base is deliberately bogus — mismatched value must not block resume
    # when workdir exists.  The branch artifact also names a nonexistent branch to
    # prove graft never consults last_artifact_branch().
    _make_state(
        state_dir,
        lenv.repo,
        workdir=str(lenv.repo),
        worktree_base="abc123deadbeef",
        artifacts=[{"type": "branch", "name": "deleted-branch-nonexistent"}],
    )
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address-code")

    captured: dict[str, object] = {}

    def fake_spawn(cmd, cwd, env, log_path, log_mode="w"):
        captured["cmd"] = cmd
        return _FakeProc()

    monkeypatch.setattr(launcher, "_spawn_logged_process", fake_spawn)

    launcher.resume(gremlin_id, graft="address")

    cmd = list(captured["cmd"])
    assert "--resume-from" in cmd
    assert cmd[cmd.index("--resume-from") + 1] == "address-code"


def test_graft_missing_worktree_plain_resume_still_raises(lenv):
    """Plain resume (no graft) still raises if worktree is gone."""
    from gremlins import launcher

    gremlin_id = "no-worktree-plain"
    state_dir = lenv.state_root / gremlin_id
    missing_workdir = lenv.repo.parent / "missing-dir"

    _make_state(
        state_dir,
        lenv.repo,
        workdir=str(missing_workdir),
        status="stopped",
        exit_code=1,
    )
    (state_dir / "finished").unlink(missing_ok=True)
    _write_hermetic(state_dir)

    with pytest.raises(RuntimeError, match="worktree missing"):
        launcher.resume(gremlin_id)


# ---------------------------------------------------------------------------
# CLI --graft flag
# ---------------------------------------------------------------------------


def test_cli_graft_passes_to_resume(lenv, monkeypatch):
    """resume_main passes --graft value to launcher.resume."""
    import gremlins.cli.resume as cli_resume_mod
    from gremlins.cli.resume import resume_main

    gremlin_id = "cli-graft-test"
    state_dir = lenv.state_root / gremlin_id
    _make_state(state_dir, lenv.repo)
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address-code")

    captured: dict[str, object] = {}

    def fake_resume(gid, *, graft=None):
        captured["gremlin_id"] = gid
        captured["graft"] = graft

    monkeypatch.setattr(cli_resume_mod, "resume", fake_resume)

    rc = resume_main([gremlin_id, "--graft", "address"])
    assert rc == 0
    assert captured["graft"] == "address"
    assert captured["gremlin_id"] == gremlin_id


def test_cli_no_graft_passes_none(lenv, monkeypatch):
    """resume_main without --graft passes graft=None."""
    import gremlins.cli.resume as cli_resume_mod
    from gremlins.cli.resume import resume_main

    gremlin_id = "cli-no-graft-test"
    state_dir = lenv.state_root / gremlin_id
    _make_state(state_dir, lenv.repo, status="stopped", exit_code=1)
    (state_dir / "finished").unlink(missing_ok=True)

    captured: dict[str, object] = {}

    def fake_resume(gid, *, graft=None):
        captured["graft"] = graft

    monkeypatch.setattr(cli_resume_mod, "resume", fake_resume)

    rc = resume_main([gremlin_id])
    assert rc == 0
    assert captured["graft"] is None
