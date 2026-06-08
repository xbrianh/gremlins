"""Tests for gremlins/launcher.py.

Drives launch_main() and launcher.resume() directly with:
- Monkeypatched HOME and PATH (so the spawned pipeline
  finds the fake `claude` binary and the gremlins package).
- Real throwaway git repos for worktree tests.

Does NOT use subprocess.run([launch.sh, ...]) — this is the Python replacement.
"""

from __future__ import annotations

import json
import os
import pathlib
import secrets
import shutil
import subprocess
import time

import pytest
from conftest import _init_git_repo
from fixtures.shell_env import install_fake_bin

import gremlins.utils.git as git_mod
from gremlins.cli.launch import launch_main

FAKE_GH = pathlib.Path(__file__).resolve().parent / "fixtures" / "fake_gh.py"


def _wait_for_finished(state_dir: pathlib.Path, timeout: float = 60.0) -> bool:
    finished = state_dir / "finished"
    sf = state_dir / "state.json"
    deadline = time.time() + timeout
    while time.time() < deadline:
        if finished.exists():
            try:
                data = json.loads(sf.read_text(encoding="utf-8"))
                if data.get("status") not in ("running", "starting", None, ""):
                    return True
            except Exception:
                pass
        time.sleep(0.1)
    return False


def _read_state(state_dir: pathlib.Path) -> dict:
    return json.loads((state_dir / "state.json").read_text(encoding="utf-8"))


@pytest.fixture
def lenv_with_gh(tmp_path, monkeypatch, lenv):
    """Like lenv but also installs a fake `gh` binary and a bare origin."""
    install_fake_bin(lenv.bin_dir, "gh", FAKE_GH)
    # Re-init the repo with a bare origin (deletes the old repo dir, so re-chdir).
    shutil.rmtree(lenv.repo, ignore_errors=True)
    _init_git_repo(lenv.repo, with_origin=True)
    monkeypatch.chdir(lenv.repo)
    return lenv


# ---------------------------------------------------------------------------
# Helpers to import launcher after env is patched
# ---------------------------------------------------------------------------


def _launcher():
    from gremlins import launcher

    return launcher


def _gremlins_state_root(lenv) -> pathlib.Path:
    return lenv.state_root


def _new_gremlin_id() -> str:
    return secrets.token_hex(8)


class _FakeProc:
    pid = 12345

    def poll(self):
        return None


# ---------------------------------------------------------------------------
# launch_main() — basic contracts
# ---------------------------------------------------------------------------


def test_launch_returns_gremlin_id(lenv):
    """launch_main() with a known --gremlin-id produces that id and a well-formed state dir."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "test instructions", "--gremlin-id", gremlin_id]
    )
    assert rc == 0, f"expected rc=0, got {rc}"
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )


def test_launch_creates_state_layout(lenv):
    """launch_main() creates state dir; initialize_runtime() fills in worktree and files."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "test instructions", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert state_dir.is_dir()
    sf = state_dir / "state.json"
    assert sf.exists()

    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )

    state = _read_state(state_dir)
    assert state["id"] == gremlin_id
    assert state["kind"] == "local"
    assert state["setup_kind"] == "worktree-detached"
    assert state["pipeline_path"].endswith(".yaml")
    assert "workdir" in state and state["workdir"]


def test_launch_writes_worktree(lenv):
    """localgremlin creates a named-branch worktree via initialize_runtime()."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(["local", "--instructions", "test", "--gremlin-id", gremlin_id])
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )
    state = _read_state(state_dir)
    workdir = pathlib.Path(state["workdir"])
    assert workdir.is_dir(), (
        f"worktree should exist after initialize_runtime: {workdir}"
    )
    r = subprocess.run(
        ["git", "-C", str(lenv.repo), "worktree", "list"],
        capture_output=True,
        text=True,
        check=True,
    )
    assert str(workdir) in r.stdout


def test_launch_persists_pipeline_args(lenv):
    """Pipeline-level flags are stored in state.json pipeline_args; pipeline path in pipeline_path."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "local",
            "--client",
            "claude:opus",
            "--instructions",
            "test",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["pipeline_path"].endswith(".yaml")
    assert state["pipeline_args"] == ["--client", "claude:opus"]


def test_launch_persists_pipeline_default_client(lenv):
    """The resolved pipeline default client is stored in state.json."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "test default client", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["client"] == "claude:sonnet"


def test_launch_persists_cli_client_space_form(lenv):
    """A space-separated --client flag overrides the stored default client."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "local",
            "--client",
            "copilot:gpt-5.4",
            "--bypass",
            "--instructions",
            "test cli client space",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_persists_cli_client_equals_form(lenv):
    """An equals-form --client flag overrides the stored default client."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "local",
            "--client=copilot:gpt-5.4",
            "--bypass",
            "--instructions",
            "test cli client equals",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_persists_last_repeated_cli_client(lenv):
    """Repeated --client flags follow argparse's last-value-wins behavior."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "local",
            "--client",
            "claude:sonnet",
            "--client=copilot:gpt-5.4",
            "--bypass",
            "--instructions",
            "test repeated cli client",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_custom_pipeline_default_client(lenv, tmp_path):
    """A custom pipeline via --pipeline stores the effective provider/model label."""
    pipeline_yaml = tmp_path / "custom.yaml"
    pipeline_yaml.write_text(
        """\
name: custom
default_client: copilot:gpt-5.4
inputs:
  in:
    INSTRUCTIONS: instructions?
stages:
  - type: exec
    options:
      cmds:
        - "true"
""",
        encoding="utf-8",
    )
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            f"--pipeline={pipeline_yaml}",
            "--instructions",
            "test custom pipeline client",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_ghgremlin_persists_pipeline_default_client(lenv_with_gh):
    """ghgremlin stores the default provider/model label."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["gh", "--instructions", "test gh default client", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv_with_gh) / gremlin_id)
    assert state["client"] == "claude:sonnet"


def test_launch_ghgremlin_persists_cli_client_override(lenv_with_gh):
    """ghgremlin stores an explicit --client override."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "gh",
            "--client",
            "copilot:gpt-5.4",
            "--bypass",
            "--instructions",
            "test gh cli client",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv_with_gh) / gremlin_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_invalid_pipeline_name_raises(lenv):
    """launch_main() returns non-zero for an unresolvable pipeline name."""
    rc = launch_main(["notapipeline", "--instructions", "test"])
    assert rc != 0


def test_launch_spawned_process_detached(lenv):
    """The spawned pipeline has a different process group than the parent."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "pgid test", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    state = _read_state(state_dir)
    pid = state.get("pid")
    assert pid is not None and pid != "null"
    try:
        child_pgid = os.getpgid(int(pid))
        parent_pgid = os.getpgrp()
        assert child_pgid != parent_pgid, (
            f"child pgid {child_pgid} should differ from parent pgid {parent_pgid}"
        )
    except ProcessLookupError:
        pass  # already exited; that's fine


def test_launch_multiple_explicit_ids(lenv):
    """Multiple launches with distinct explicit IDs succeed."""
    gremlin_ids = [_new_gremlin_id() for _ in range(5)]
    state_root = _gremlins_state_root(lenv)
    for i, gremlin_id in enumerate(gremlin_ids):
        rc = launch_main(
            ["local", "--instructions", f"concurrent {i}", "--gremlin-id", gremlin_id]
        )
        assert rc == 0
    assert len(set(gremlin_ids)) == len(gremlin_ids), (
        f"GREMLIN_ID collision among: {gremlin_ids}"
    )
    for gremlin_id in gremlin_ids:
        _wait_for_finished(state_root / gremlin_id)


def test_launch_explicit_project_root(lenv):
    """Explicit parent_id is recorded in state.json."""
    gremlin_id = _new_gremlin_id()
    parent_id = "fake-parent-aabbcc"
    rc = launch_main(
        [
            "local",
            "--instructions",
            "child test",
            "--parent",
            parent_id,
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state_root = _gremlins_state_root(lenv)
    state = _read_state(state_root / gremlin_id)
    assert (
        pathlib.Path(state["project_root"]).resolve()
        == pathlib.Path(lenv.repo).resolve()
    )
    assert state["parent_id"] == parent_id


# ---------------------------------------------------------------------------
# resume() — state patches and guards
# ---------------------------------------------------------------------------


def test_resume_patches_state(lenv, monkeypatch):
    """Manual resume() clears markers and patches state.json."""
    launcher = _launcher()
    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "plan")
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "test resume", "--gremlin-id", gremlin_id]
    )
    assert rc != 0  # gremlin exits early with failure
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=30), (
        "failed gremlin should terminate quickly"
    )

    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "")
    launcher.resume(gremlin_id)

    post_state = _read_state(state_dir)
    assert post_state["status"] == "running"
    assert post_state["resumed_from_stage"] == "plan"
    assert post_state["pipeline_path"].endswith(".yaml")
    assert not (state_dir / "finished").exists(), "finished marker must be cleared"

    _wait_for_finished(state_dir, timeout=60)


def test_resume_uses_persisted_client_label(lenv, monkeypatch):
    """resume() propagates the persisted client field into the updated state."""
    old_pipeline = lenv.repo / "old.yaml"
    old_pipeline.write_text(
        """\
name: old
default_client: copilot:gpt-5.4
stages:
  - name: plan
    type: agent
  - name: implement
    type: agent
    client: claude:opus
""",
        encoding="utf-8",
    )
    launcher = _launcher()
    gremlin_id = "resume-stage-client"
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    state_dir.mkdir(parents=True)
    (state_dir / "log").write_text("", encoding="utf-8")
    (state_dir / "finished").touch()
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "kind": "localgremlin",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "implement",
                "status": "stopped",
                "exit_code": 1,
                "client": "claude:opus",
                "pipeline_args": ["--pipeline", str(old_pipeline)],
                "pipeline_path": str(old_pipeline),
            }
        ),
        encoding="utf-8",
    )

    class _Proc:
        pid = 12345

    monkeypatch.setattr(
        launcher, "_spawn_logged_process", lambda *args, **kwargs: _Proc()
    )

    launcher.resume(gremlin_id)

    post_state = _read_state(state_dir)
    assert post_state["client"] == "claude:opus"
    assert post_state["status"] == "running"
    assert post_state["resumed_from_stage"] == "implement"
    assert post_state["pipeline_path"] == str(old_pipeline.resolve())
    assert not (state_dir / "finished").exists()


def test_resume_refuses_running_gremlin(lenv):
    """resume() raises RuntimeError when the recorded pid is still alive."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    gremlin_id = "fake-running-deadbe"
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "workdir": str(lenv.repo),
        "branch": f"bg/local/{gremlin_id}",
        "stage": "plan",
        "status": "running",
        "pid": os.getpid(),
        "pipeline_args": [],
    }
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")

    with pytest.raises(RuntimeError, match="still running"):
        launcher.resume(gremlin_id)


def test_resume_refuses_finished_success(lenv):
    """resume() raises RuntimeError for a gremlin that already succeeded."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    gremlin_id = "fake-done-success"
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "workdir": str(lenv.repo),
        "branch": f"bg/local/{gremlin_id}",
        "stage": "address-code",
        "status": "done",
        "exit_code": 0,
        "pipeline_args": [],
    }
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")
    (state_dir / "finished").touch()

    with pytest.raises(RuntimeError, match="finished successfully"):
        launcher.resume(gremlin_id)


# ---------------------------------------------------------------------------
# _run-pipeline subcommand (terminal state)
# ---------------------------------------------------------------------------


def test_run_pipeline_writes_terminal_state_on_success(lenv, monkeypatch):
    """_run-pipeline writes exit_code=0 + status=done + finished marker on success."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text(
        "# Test Plan\n\n## Tasks\n- [ ] Touch a file\n", encoding="utf-8"
    )
    gremlin_id = _new_gremlin_id()
    rc = launch_main(["local", "--plan", str(plan_file), "--gremlin-id", gremlin_id])
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=120), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )
    state = _read_state(state_dir)
    assert state["exit_code"] == 0
    assert state["status"] == "done"
    assert (state_dir / "finished").exists()


def test_run_pipeline_writes_terminal_state_on_failure(lenv, monkeypatch):
    """_run-pipeline writes exit_code!=0 + status=stopped + finished marker on failure."""
    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "plan")
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "fail test", "--gremlin-id", gremlin_id]
    )
    assert rc != 0  # gremlin exits early with failure
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=60), (
        "pipeline should terminate quickly on failure"
    )
    state = _read_state(state_dir)
    assert state["exit_code"] != 0
    assert state["status"] == "stopped"
    assert (state_dir / "finished").exists()


def test_write_terminal_state_preserves_worktree_for_gh(lenv, monkeypatch, tmp_path):
    """On success, worktree is NOT removed for gh-mode pipelines (only explicit close/land removes it)."""
    from gremlins.executor.state import StateData

    removed = []
    monkeypatch.setattr(
        "gremlins.utils.git.remove_worktree", lambda root, wd: removed.append(wd)
    )

    state_dir = lenv.state_root / "test-gr-id-abc123"
    state_dir.mkdir(parents=True, exist_ok=True)
    fake_workdir = tmp_path / "workdir"
    fake_workdir.mkdir()
    state_json = {
        "project_root": str(lenv.repo),
        "workdir": str(fake_workdir),
    }
    (state_dir / "state.json").write_text(json.dumps(state_json), encoding="utf-8")

    StateData.load("test-gr-id-abc123").write_terminal_state(0)

    assert removed == [], "worktree must not be removed on exit"


def test_write_terminal_state_preserves_worktree_for_local(lenv, monkeypatch, tmp_path):
    """On success, worktree is NOT removed for local-mode pipelines."""
    from gremlins.executor.state import StateData

    removed = []
    monkeypatch.setattr(
        "gremlins.utils.git.remove_worktree", lambda root, wd: removed.append(wd)
    )

    state_dir = lenv.state_root / "test-gr-id-def456"
    state_dir.mkdir(parents=True, exist_ok=True)
    fake_workdir = tmp_path / "workdir"
    fake_workdir.mkdir()
    state_json = {
        "project_root": str(lenv.repo),
        "workdir": str(fake_workdir),
        "setup_kind": "worktree-branch",
    }
    (state_dir / "state.json").write_text(json.dumps(state_json), encoding="utf-8")

    StateData.load("test-gr-id-def456").write_terminal_state(0)

    assert removed == [], "worktree must not be removed for local-mode pipelines"


def test_write_terminal_state_preserves_worktree_for_boss(lenv, monkeypatch, tmp_path):
    """On success, worktree is NOT removed for boss-mode pipelines."""
    from gremlins.executor.state import StateData

    removed = []
    monkeypatch.setattr(
        "gremlins.utils.git.remove_worktree", lambda root, wd: removed.append(wd)
    )

    state_dir = lenv.state_root / "test-gr-id-ghi789"
    state_dir.mkdir(parents=True, exist_ok=True)
    fake_workdir = tmp_path / "workdir"
    fake_workdir.mkdir()
    state_json = {
        "project_root": str(lenv.repo),
        "workdir": str(fake_workdir),
        "setup_kind": "worktree-detached",
    }
    (state_dir / "state.json").write_text(json.dumps(state_json), encoding="utf-8")

    StateData.load("test-gr-id-ghi789").write_terminal_state(0)

    assert removed == [], "worktree must not be removed on exit"


# ---------------------------------------------------------------------------
# Full pipeline smoke test
# ---------------------------------------------------------------------------


def test_full_localgremlin_pipeline(lenv, monkeypatch):
    """plan → implement → review → address all run once in order."""
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["local", "--instructions", "test full pipeline", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=120), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )
    state = _read_state(state_dir)
    assert state["status"] == "done", (
        f"expected done, got {state.get('status')}; log:\n"
        f"{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )
    assert state["exit_code"] == 0
    # Check stage ordering from fake claude log
    if lenv.fake_claude_log.exists():
        log = [
            json.loads(line)
            for line in lenv.fake_claude_log.read_text().splitlines()
            if line.strip()
        ]
        stages = [e["stage"] for e in log]
        assert stages[0] == "plan", stages
        assert "implement-local" in stages, stages
        assert "review" in stages, stages
        assert "address" in stages, stages


# ---------------------------------------------------------------------------
# ghgremlin launch (lenv_with_gh fixture)
# ---------------------------------------------------------------------------


def test_launch_ghgremlin_state_layout(lenv_with_gh):
    """ghgremlin creates a detached worktree off origin/<default> with correct state."""
    lenv = lenv_with_gh
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        ["gh", "--instructions", "test gh launch", "--gremlin-id", gremlin_id]
    )
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert state_dir.is_dir()

    assert _wait_for_finished(state_dir, timeout=120), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )

    state = _read_state(state_dir)
    assert state["id"] == gremlin_id
    assert state["kind"] == "gh"
    assert state["setup_kind"] == "worktree-detached", (
        f"ghgremlin should use detached worktree, got: {state['setup_kind']!r}"
    )
    assert "base_ref_name" not in state, (
        f"base_ref_name should not be in state.json (moved to registry), got: {state.get('base_ref_name')!r}"
    )
    registry_data = json.loads(
        (_gremlins_state_root(lenv) / gremlin_id / "registry.json").read_text()
    )
    assert registry_data.get("base_ref") == "git://ref/main", (
        f"base_ref should be 'git://ref/main' in registry, got: {registry_data.get('base_ref')!r}"
    )
    _sha_uri = registry_data.get("base_sha", "")
    assert (
        _sha_uri.startswith("git://commit/")
        and len(_sha_uri.removeprefix("git://commit/")) == 40
    ), f"base_sha should be a git://commit/<40-char SHA>, got: {_sha_uri!r}"
    assert len(state.get("worktree_base", "")) == 40, (
        f"worktree_base should be a SHA, got: {state.get('worktree_base')!r}"
    )
    workdir = pathlib.Path(state["workdir"])
    assert workdir.is_dir(), f"worktree directory should exist: {workdir}"


# ---------------------------------------------------------------------------
# PYTHONSAFEPATH worktree-rename regression
# ---------------------------------------------------------------------------


def test_launch_passes_base_ref_to_worktree_setup(lenv):
    """launch_main(--base-ref <sha>) passes the sha to the worktree setup and persists it."""
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        cwd=str(lenv.repo),
        check=True,
    )
    head_sha = r.stdout.strip()

    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "local",
            "--instructions",
            "base_ref test",
            "--base-ref",
            head_sha,
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )

    state = _read_state(state_dir)
    registry_data = json.loads(
        (_gremlins_state_root(lenv) / gremlin_id / "registry.json").read_text()
    )
    assert registry_data.get("base_sha") == f"git://commit/{head_sha}", (
        f"expected base_sha=git://commit/{head_sha!r}, got {registry_data.get('base_sha')!r}"
    )
    workdir_base = state.get("worktree_base", "")
    assert workdir_base == head_sha, (
        f"worktree should be created at base_ref_sha={head_sha!r}, got {workdir_base!r}"
    )


def test_pipeline_survives_worktree_pipeline_rename(lenv, monkeypatch):
    """Regression: pipeline completes even when implement renames worktree's gremlins/.

    Without PYTHONSAFEPATH=1, python -m gremlins.cli imports from the worktree
    (cwd). Renaming gremlins/ during implement then causes ImportError / FileNotFoundError
    in later stages because PROMPTS_DIR is __file__-relative and the directory is gone.
    With the fix, python loads gremlins from the install root (derived via __file__)
    and the worktree rename is harmless.
    """
    # Add a gremlins/ stub to the repo so the worktree cwd shadows the install root.
    pipeline_stub = lenv.repo / "gremlins"
    pipeline_stub.mkdir()
    (pipeline_stub / "__init__.py").write_text("# stub\n", encoding="utf-8")
    subprocess.run(
        ["git", "-C", str(lenv.repo), "add", "gremlins"],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(lenv.repo), "commit", "-m", "add gremlins stub"],
        check=True,
        capture_output=True,
    )

    # FAKE_CLAUDE_RENAME_GREMLINS=1 triggers the rename inside handle_implement()
    # (see fixtures/fake_claude.py). Set it in the current env so the spawned
    # pipeline process inherits it via os.environ.copy() in _build_spawn_env().
    monkeypatch.setenv("FAKE_CLAUDE_RENAME_GREMLINS", "1")

    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            "local",
            "--instructions",
            "test gremlins rename regression",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    log_path = state_dir / "log"
    assert _wait_for_finished(state_dir, timeout=120), (
        f"pipeline did not finish; log:\n"
        f"{log_path.read_text(errors='replace')[-2000:] if log_path.exists() else '<log missing>'}"
    )
    state = _read_state(state_dir)
    assert state["exit_code"] == 0, (
        f"expected exit 0; status={state.get('status')!r}; log tail:\n"
        f"{log_path.read_text(errors='replace')[-2000:] if log_path.exists() else '<log missing>'}"
    )


# ---------------------------------------------------------------------------
# .gremlins overlay placement
# ---------------------------------------------------------------------------


def test_setup_workdir_overlay_goes_to_state_dir(lenv):
    """stage_gremlins_overlay copies .gremlins to state_dir, not the worktree."""
    overlay_src = lenv.repo / ".gremlins"
    overlay_src.mkdir(parents=True)
    (overlay_src / "custom-local.yaml").write_text(
        "name: custom-local\nstages: []\n", encoding="utf-8"
    )

    gremlin_id = "overlay-test-aabbcc"
    state_dir = lenv.state_root / gremlin_id
    state_dir.mkdir(parents=True)

    workdir = git_mod.setup_workdir(str(lenv.repo), "HEAD", state_dir=state_dir)

    try:
        assert (state_dir / ".gremlins" / "custom-local.yaml").exists()
        assert not (pathlib.Path(workdir) / ".gremlins").exists()
        r = subprocess.run(
            ["git", "-C", workdir, "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=True,
        )
        assert r.stdout.strip() == "", f"worktree is not clean:\n{r.stdout}"
    finally:
        git_mod.remove_worktree(str(lenv.repo), workdir)


def test_setup_workdir_detached_with_fetch(tmp_path):
    """setup_workdir with fetch=True fetches the ref from origin and creates detached worktree."""
    repo = tmp_path / "repo"
    _init_git_repo(repo, with_origin=True)

    feature = "feature/wkdir-ref"
    subprocess.run(
        ["git", "checkout", "-b", feature], cwd=repo, check=True, capture_output=True
    )
    (repo / "wkdir.txt").write_text("wkdir\n")
    subprocess.run(
        ["git", "add", "wkdir.txt"], cwd=repo, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "wkdir commit"],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "push", "origin", feature], cwd=repo, check=True, capture_output=True
    )
    feature_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    subprocess.run(
        ["git", "checkout", "main"], cwd=repo, check=True, capture_output=True
    )

    state_dir = tmp_path / "state"
    state_dir.mkdir()

    workdir = git_mod.setup_workdir(str(repo), feature, fetch=True, state_dir=state_dir)
    try:
        wt_sha = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=workdir,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        assert wt_sha == feature_sha
    finally:
        git_mod.remove_worktree(str(repo), workdir)


def test_setup_workdir_non_git_raises(tmp_path):
    """setup_workdir raises GitError when project_root is not a git repository."""
    non_repo = tmp_path / "not-a-git-repo"
    non_repo.mkdir()
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    with pytest.raises(git_mod.GitError) as exc_info:
        git_mod.setup_workdir(str(non_repo), "HEAD", state_dir=state_dir)

    assert exc_info.value.returncode == 128
    assert "is not a git repository" in exc_info.value.stderr


# ---------------------------------------------------------------------------
# stage_inputs persistence
# ---------------------------------------------------------------------------


def test_launch_persists_stage_inputs(lenv, tmp_path):
    """stage_inputs dict is written verbatim to state.json."""
    pipeline_yaml = tmp_path / "test_pipeline.yaml"
    pipeline_yaml.write_text(
        """\
name: test_pipeline
default_client: claude:sonnet

inputs:
  in:
    INSTRUCTIONS: instructions?
    EXTRA_KEY: extra_key?

stages:
  - type: exec
    options:
      cmds:
        - "true"
""",
        encoding="utf-8",
    )
    gremlin_id = _new_gremlin_id()
    rc = launch_main(
        [
            f"--pipeline={pipeline_yaml}",
            "--instructions",
            "do the thing",
            "--extra-key",
            "val",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / gremlin_id)
    assert state["stage_inputs"] == {
        "instructions": "do the thing",
        "extra_key": "val",
    }


def test_stage_inputs_survives_resume(lenv, monkeypatch):
    """stage_inputs written at launch is still present in state.json after resume."""
    launcher = _launcher()
    gremlin_id = "resume-stage-inputs-roundtrip"
    state_dir = _gremlins_state_root(lenv) / gremlin_id
    state_dir.mkdir(parents=True)
    (state_dir / "log").write_text("", encoding="utf-8")
    (state_dir / "finished").touch()
    saved_stage_inputs = {"instructions": "do the thing", "flag": "value"}
    pipeline_yaml = lenv.repo / "pipeline.yaml"
    pipeline_yaml.write_text(
        "stages:\n  - name: plan\n    type: agent\n", encoding="utf-8"
    )
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "kind": "localgremlin",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "plan",
                "status": "stopped",
                "exit_code": 1,
                "pipeline_args": ["--pipeline", str(pipeline_yaml)],
                "pipeline_path": str(pipeline_yaml),
                "stage_inputs": saved_stage_inputs,
            }
        ),
        encoding="utf-8",
    )

    class _Proc:
        pid = 99999

    monkeypatch.setattr(
        launcher, "_spawn_logged_process", lambda *args, **kwargs: _Proc()
    )

    launcher.resume(gremlin_id)

    post_state = _read_state(state_dir)
    assert post_state["stage_inputs"] == saved_stage_inputs


# ---------------------------------------------------------------------------
# --gremlin-id explicit id
# ---------------------------------------------------------------------------


def test_launch_explicit_gremlin_id(lenv):
    """launch_main(--gremlin-id ...) uses the supplied id verbatim."""
    rc = launch_main(
        [
            "local",
            "--instructions",
            "explicit id test",
            "--gremlin-id",
            "my-explicit-id",
        ]
    )
    assert rc == 0
    state = _read_state(_gremlins_state_root(lenv) / "my-explicit-id")
    assert state["id"] == "my-explicit-id"


def test_launch_invalid_gremlin_id_rejected(lenv, monkeypatch):
    """launch_main(--gremlin-id ...) returns non-zero for ids that fail validate_gremlin_id."""
    rc = launch_main(
        [
            "local",
            "--instructions",
            "bad id test",
            "--gremlin-id",
            "bad id with spaces",
        ]
    )
    assert rc != 0


def test_launch_gremlin_id_pipeline_name_rejected(lenv, monkeypatch):
    """launch_main(--gremlin-id ...) returns non-zero when id matches a pipeline name."""
    rc = launch_main(
        [
            "local",
            "--instructions",
            "pipeline collision test",
            "--gremlin-id",
            "local",
        ]
    )
    assert rc != 0


def test_launch_explicit_gremlin_id_already_running(lenv, monkeypatch):
    """launch_main(--gremlin-id ...) returns non-zero when the id has a live process."""
    state_root = _gremlins_state_root(lenv)
    gremlin_id = "my-fixed-id"
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(
        json.dumps({"id": gremlin_id, "status": "running", "pid": os.getpid()}),
        encoding="utf-8",
    )
    rc = launch_main(
        [
            "local",
            "--instructions",
            "collision test",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc != 0


def test_launch_explicit_gremlin_id_stale_dir_refused(lenv, monkeypatch):
    """launch_main(--gremlin-id ...) returns non-zero when the state dir exists but the process is not running."""
    state_root = _gremlins_state_root(lenv)
    gremlin_id = "my-fixed-id"
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(
        json.dumps({"id": gremlin_id, "status": "running", "pid": 0}),
        encoding="utf-8",
    )
    rc = launch_main(
        [
            "local",
            "--instructions",
            "stale dir test",
            "--gremlin-id",
            gremlin_id,
        ]
    )
    assert rc != 0
