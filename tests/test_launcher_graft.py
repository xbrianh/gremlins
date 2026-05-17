"""Tests for gremlins resume --graft."""

from __future__ import annotations

import json
import os
import pathlib

import pytest
import yaml

# ---------------------------------------------------------------------------
# _next_graft_name — unit tests
# ---------------------------------------------------------------------------


def test_next_graft_name_empty():
    from gremlins.launcher import _next_graft_name

    assert _next_graft_name([]) == "graft-1"


def test_next_graft_name_first_graft():
    from gremlins.launcher import _next_graft_name

    stages = [{"name": "plan"}, {"name": "implement"}]
    assert _next_graft_name(stages) == "graft-1"


def test_next_graft_name_increments():
    from gremlins.launcher import _next_graft_name

    stages = [{"name": "graft-1"}, {"name": "graft-2"}]
    assert _next_graft_name(stages) == "graft-3"


def test_next_graft_name_mixed():
    from gremlins.launcher import _next_graft_name

    stages = [{"name": "plan"}, {"name": "graft-1"}, {"name": "implement"}]
    assert _next_graft_name(stages) == "graft-2"


def test_next_graft_name_noncontiguous():
    from gremlins.launcher import _next_graft_name

    # graft-2 present without graft-1 — must not return graft-2 (duplicate)
    stages = [{"name": "graft-2"}, {"name": "graft-3"}]
    assert _next_graft_name(stages) == "graft-4"


# ---------------------------------------------------------------------------
# _append_graft — unit tests
# ---------------------------------------------------------------------------


def _write_pipeline(path: pathlib.Path, stages: list) -> None:
    path.write_text(
        yaml.dump({"__gremlins_expanded__": True, "stages": stages}),
        encoding="utf-8",
    )


def test_append_graft_adds_sequence(tmp_path):
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

    _append_graft(hermetic.parent, "address", str(tmp_path))

    loaded = yaml.safe_load(hermetic.read_text(encoding="utf-8"))
    top = loaded["stages"]
    assert len(top) == 2
    assert top[1]["name"] == "graft-1"
    assert top[1]["type"] == "sequence"
    assert top[1]["body"] == [{"name": "address", "type": "address"}]


def test_append_graft_increments_name(tmp_path):
    from gremlins.launcher import _append_graft

    hermetic = tmp_path / "state" / "pipeline.yaml"
    hermetic.parent.mkdir()
    _write_pipeline(
        hermetic,
        [
            {"name": "plan", "type": "plan"},
            {"name": "graft-1", "type": "sequence", "body": [{"name": "x"}]},
        ],
    )

    graft_yaml = tmp_path / ".gremlins" / "review.yaml"
    graft_yaml.parent.mkdir()
    graft_yaml.write_text(
        yaml.dump({"stages": [{"name": "review", "type": "review"}]}),
        encoding="utf-8",
    )

    _append_graft(hermetic.parent, "review", str(tmp_path))

    loaded = yaml.safe_load(hermetic.read_text(encoding="utf-8"))
    assert loaded["stages"][-1]["name"] == "graft-2"


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
    assert cmd[cmd.index("--resume-from") + 1] == "graft-1"


def test_graft_appends_wrapped_stage(lenv, monkeypatch):
    """resume(graft=...) appends a graft-N sequence to the hermetic pipeline.yaml."""
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
    graft_stage = top[-1]
    assert graft_stage["name"] == "graft-1"
    assert graft_stage["type"] == "sequence"
    assert graft_stage["body"][0]["name"] == "address-code"


def test_repeated_grafts_increment_names(lenv, monkeypatch):
    """Two grafts produce graft-1 then graft-2 with no collisions."""
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
    assert "graft-1" in names
    assert "graft-2" in names


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
    # Write hermetic pipeline that already has a graft-1 stage
    _write_hermetic(
        state_dir,
        [
            {"name": "plan", "type": "plan"},
            {"name": "graft-1", "type": "sequence", "body": [{"name": "address-code"}]},
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


def test_graft_missing_worktree_no_recovery_raises(lenv, monkeypatch):
    """resume(graft=...) raises if worktree is gone and cannot be recreated."""
    import gremlins.fleet.rescue as rescue_mod
    from gremlins import launcher

    gremlin_id = "graft-no-worktree-test"
    state_dir = lenv.state_root / gremlin_id
    missing_workdir = lenv.repo.parent / "nonexistent-worktree"

    _make_state(state_dir, lenv.repo, workdir=str(missing_workdir))
    _write_hermetic(state_dir)
    _write_graft_pipeline(lenv.repo, "address", "address-code")

    monkeypatch.setattr(launcher, "_spawn_logged_process", lambda *a, **kw: _FakeProc())
    monkeypatch.setattr(
        rescue_mod, "recreate_worktree", lambda s: (False, "not a repo")
    )

    with pytest.raises(
        RuntimeError, match="worktree missing and could not be recreated"
    ):
        launcher.resume(gremlin_id, graft="address")

    # State must not be mutated (finished marker still present)
    assert (state_dir / "finished").exists()


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
