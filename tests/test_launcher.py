"""Tests for gremlins/launcher.py.

Drives launcher.launch() and launcher.resume() directly with:
- Monkeypatched HOME, XDG_STATE_HOME, PATH (so the spawned pipeline
  finds the fake `claude` binary and the gremlins package).
- Real throwaway git repos for worktree tests.

Does NOT use subprocess.run([launch.sh, ...]) — this is the Python replacement.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import shlex
import subprocess
import sys
import time

import pytest

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
FIXTURES_DIR = pathlib.Path(__file__).resolve().parent / "fixtures"
FAKE_CLAUDE = FIXTURES_DIR / "fake_claude.py"
FAKE_GH = FIXTURES_DIR / "fake_gh.py"


def _install_fake_bin(bin_dir: pathlib.Path, name: str, target: pathlib.Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    wrapper = bin_dir / name
    wrapper.write_text(
        f"#!/usr/bin/env bash\nexec {shlex.quote(sys.executable)} "
        f"{shlex.quote(str(target))} \"$@\"\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o755)


def _setup_claude_home(home: pathlib.Path) -> None:
    claude_dir = home / ".claude"
    claude_dir.mkdir(parents=True, exist_ok=True)
    for name in ("gremlins", "agents"):
        link = claude_dir / name
        if not link.exists() and not link.is_symlink():
            link.symlink_to(REPO_ROOT / name)


def _init_git_repo(path: pathlib.Path, *, with_origin: bool = False) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "test@test.com"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=path, check=True, capture_output=True)
    (path / "README.md").write_text("init\n")
    subprocess.run(["git", "add", "README.md"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True)
    if with_origin:
        bare = path.parent / f"{path.name}.git"
        subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True, capture_output=True)
        subprocess.run(["git", "remote", "add", "origin", str(bare)], cwd=path, check=True, capture_output=True)
        subprocess.run(["git", "push", "-u", "origin", "main"], cwd=path, check=True, capture_output=True)


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


def _wait_for_workdir_removed(workdir: pathlib.Path, timeout: float = 30.0) -> bool:
    # The `finished` marker is touched before worktree cleanup runs (the marker
    # ordering suppresses session-summary's crashed-detection race), so a test
    # that asserts on cleanup must wait for the workdir separately.
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not workdir.exists():
            return True
        time.sleep(0.1)
    return False


def _read_state(state_dir: pathlib.Path) -> dict:
    return json.loads((state_dir / "state.json").read_text(encoding="utf-8"))


@pytest.fixture
def lenv(tmp_path, monkeypatch):
    """Launcher environment: isolated HOME, state root, git repo, fake claude."""
    home = tmp_path / "home"
    home.mkdir(parents=True, exist_ok=True)
    _setup_claude_home(home)

    bin_dir = tmp_path / "bin"
    _install_fake_bin(bin_dir, "claude", FAKE_CLAUDE)

    state_root = tmp_path / "state"
    state_root.mkdir(parents=True, exist_ok=True)

    repo = tmp_path / "repo"
    _init_git_repo(repo)

    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("XDG_STATE_HOME", str(state_root))
    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(tmp_path / "fake_claude.log"))
    monkeypatch.setenv("GIT_OPTIONAL_LOCKS", "0")
    old_path = os.environ.get("PATH", "")
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{old_path}")
    monkeypatch.delenv("PYTHONPATH", raising=False)
    monkeypatch.delenv("GR_ID", raising=False)
    monkeypatch.chdir(repo)

    class _Env:
        pass
    e = _Env()
    e.home = home
    e.bin_dir = bin_dir
    e.state_root = state_root
    e.repo = repo
    e.fake_claude_log = tmp_path / "fake_claude.log"
    return e


@pytest.fixture
def lenv_with_gh(tmp_path, monkeypatch, lenv):
    """Like lenv but also installs a fake `gh` binary and a bare origin."""
    _install_fake_bin(lenv.bin_dir, "gh", FAKE_GH)
    # Re-init the repo with a bare origin (deletes the old repo dir, so re-chdir).
    import shutil
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
    return lenv.state_root / "claude-gremlins"


# ---------------------------------------------------------------------------
# launch() — basic contracts
# ---------------------------------------------------------------------------

def test_launch_returns_gr_id(lenv):
    """launch() returns a well-formed GR_ID string."""
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", instructions="test instructions")
    assert gr_id, "expected a non-empty GR_ID"
    assert re.match(r"^[a-z0-9-]+-[0-9a-f]{6}$", gr_id), \
        f"GR_ID has unexpected shape: {gr_id!r}"
    state_dir = _gremlins_state_root(lenv) / gr_id
    _wait_for_finished(state_dir, timeout=60)


def test_launch_creates_state_layout(lenv):
    """launch() creates state dir with all required files and state.json fields."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "localgremlin", instructions="test instructions",
        pipeline_args=("-i", "sonnet"),
    )
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert state_dir.is_dir()
    sf = state_dir / "state.json"
    assert sf.exists()
    assert (state_dir / "instructions.txt").exists()
    assert (state_dir / "instructions.txt").read_text(encoding="utf-8") == "test instructions"

    state = _read_state(state_dir)
    assert state["id"] == gr_id
    assert state["kind"] == "localgremlin"
    assert state["setup_kind"] == "worktree-branch"
    assert state["branch"] == f"bg/localgremlin/{gr_id}"
    assert state["pipeline_args"] == ["-i", "sonnet"]
    assert "test instructions" in state["instructions"]
    assert "workdir" in state and state["workdir"]
    _wait_for_finished(state_dir, timeout=60)


def test_launch_writes_worktree(lenv):
    """localgremlin creates a named-branch worktree immediately after launch."""
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", instructions="test")
    state_dir = _gremlins_state_root(lenv) / gr_id
    state = _read_state(state_dir)
    workdir = pathlib.Path(state["workdir"])
    assert workdir.is_dir(), f"worktree should exist right after launch: {workdir}"
    r = subprocess.run(
        ["git", "-C", str(lenv.repo), "worktree", "list"],
        capture_output=True, text=True, check=True,
    )
    assert str(workdir) in r.stdout
    _wait_for_finished(state_dir, timeout=60)


def test_launch_persists_pipeline_args(lenv):
    """Pipeline-level flags are stored verbatim in state.json pipeline_args."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "localgremlin",
        pipeline_args=("-p", "opus", "-i", "sonnet"),
        instructions="test",
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["pipeline_args"] == ["-p", "opus", "-i", "sonnet"]
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_persists_impl_model_explicit(lenv):
    """impl_model is extracted from -i flag and stored in state.json."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "localgremlin",
        pipeline_args=("-i", "opus"),
        instructions="test impl model",
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["impl_model"] == "opus"
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_persists_impl_model_default(lenv):
    """impl_model defaults to 'sonnet' when no -i flag is supplied."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "localgremlin",
        instructions="test impl model default",
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["impl_model"] == "sonnet"
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_persists_impl_model_gh_space_form(lenv_with_gh):
    """impl_model is extracted from space-separated --model flag for ghgremlin."""
    lenv = lenv_with_gh
    launcher = _launcher()
    gr_id = launcher.launch(
        "ghgremlin",
        pipeline_args=("--model", "opus"),
        instructions="test gh model space",
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["impl_model"] == "opus"
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_persists_impl_model_gh_equals_form(lenv_with_gh):
    """impl_model is extracted from --model=<value> flag for ghgremlin."""
    lenv = lenv_with_gh
    launcher = _launcher()
    gr_id = launcher.launch(
        "ghgremlin",
        pipeline_args=("--model=haiku",),
        instructions="test gh model equals",
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["impl_model"] == "haiku"
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_plan_normalized_to_absolute(lenv):
    """A relative --plan path is resolved to absolute in state.json."""
    plan_file = lenv.repo / "my-plan.md"
    plan_file.write_text("# My Plan Heading\n\nBody.\n", encoding="utf-8")
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", plan=str(plan_file.name))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    idx = state["pipeline_args"].index("--plan")
    persisted = state["pipeline_args"][idx + 1]
    assert os.path.isabs(persisted), f"expected absolute path, got: {persisted!r}"
    assert pathlib.Path(persisted).name == "my-plan.md"
    assert state["description"].startswith("My Plan Heading")
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_h1_as_description(lenv):
    """H1 from --plan file becomes the gremlin description."""
    plan_file = lenv.repo / "plan-with-h1.md"
    plan_file.write_text("# Hello World Feature\n\n## Tasks\n- [ ] Do it\n", encoding="utf-8")
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", plan=str(plan_file))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["description"].startswith("Hello World Feature")
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_invalid_kind_raises(lenv):
    """launch() raises ValueError for an unrecognized kind."""
    launcher = _launcher()
    with pytest.raises(ValueError, match="invalid kind"):
        launcher.launch("notakind", instructions="test")


def test_launch_plan_and_instructions_mutex(lenv):
    """launch() raises ValueError when both plan and instructions are given."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# X\n", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="mutually exclusive"):
        launcher.launch("localgremlin", plan=str(plan_file), instructions="extra")


def test_launch_empty_plan_file_rejected(lenv):
    """localgremlin rejects an empty --plan file before creating state."""
    empty = lenv.repo / "empty-plan.md"
    empty.write_text("", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="empty"):
        launcher.launch("localgremlin", plan=str(empty))
    # No state dir should have been created
    dirs = list((_gremlins_state_root(lenv)).glob("*")) \
        if _gremlins_state_root(lenv).exists() else []
    assert dirs == [], f"empty-plan failure must not create state: {dirs}"


def test_launch_spawned_process_detached(lenv):
    """The spawned pipeline has a different process group than the parent."""
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", instructions="pgid test")
    state_dir = _gremlins_state_root(lenv) / gr_id
    state = _read_state(state_dir)
    pid = state.get("pid")
    assert pid is not None and pid != "null"
    try:
        child_pgid = os.getpgid(int(pid))
        parent_pgid = os.getpgrp()
        assert child_pgid != parent_pgid, \
            f"child pgid {child_pgid} should differ from parent pgid {parent_pgid}"
    except ProcessLookupError:
        pass  # already exited; that's fine
    _wait_for_finished(state_dir, timeout=60)


def test_launch_concurrent_no_collision(lenv):
    """Concurrent launches produce distinct GR_IDs."""
    launcher = _launcher()
    ids = [
        launcher.launch("localgremlin", instructions=f"concurrent {i}")
        for i in range(5)
    ]
    assert len(set(ids)) == len(ids), f"GR_ID collision among: {ids}"
    for gr_id in ids:
        _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_explicit_project_root(lenv):
    """Explicit project_root param is used; parent_id is recorded in state.json."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    parent_id = "fake-parent-aabbcc"
    gr_id = launcher.launch(
        "localgremlin", instructions="child test",
        parent_id=parent_id, project_root=str(lenv.repo),
    )
    state = _read_state(state_root / gr_id)
    assert state["project_root"] == str(lenv.repo)
    assert state["parent_id"] == parent_id
    _wait_for_finished(state_root / gr_id, timeout=60)


# ---------------------------------------------------------------------------
# resume() — state patches and guards
# ---------------------------------------------------------------------------

def test_resume_patches_state(lenv, monkeypatch):
    """resume() bumps rescue_count, clears markers, and patches state.json."""
    launcher = _launcher()
    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "plan")
    gr_id = launcher.launch("localgremlin", pipeline_args=("-i", "sonnet"), instructions="test resume")
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=30), "failed gremlin should terminate quickly"

    pre_state = _read_state(state_dir)
    pre_rescue_count = pre_state.get("rescue_count", 0)

    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "")
    launcher.resume(gr_id)

    post_state = _read_state(state_dir)
    assert post_state["rescue_count"] == pre_rescue_count + 1
    assert post_state["status"] == "running"
    assert post_state["resumed_from_stage"] == "plan"
    assert post_state["pipeline_args"] == ["-i", "sonnet"]
    assert not (state_dir / "finished").exists(), "finished marker must be cleared"

    _wait_for_finished(state_dir, timeout=60)


def test_resume_refuses_running_gremlin(lenv):
    """resume() raises RuntimeError when the recorded pid is still alive."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    gr_id = "fake-running-deadbe"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    state = {
        "id": gr_id, "kind": "localgremlin",
        "workdir": str(lenv.repo), "branch": f"bg/localgremlin/{gr_id}",
        "stage": "plan", "status": "running", "pid": os.getpid(),
        "pipeline_args": [],
    }
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")
    (state_dir / "instructions.txt").write_text("foo", encoding="utf-8")

    with pytest.raises(RuntimeError, match="still running"):
        launcher.resume(gr_id)


def test_resume_refuses_finished_success(lenv):
    """resume() raises RuntimeError for a gremlin that already succeeded."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    gr_id = "fake-done-success"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    state = {
        "id": gr_id, "kind": "localgremlin",
        "workdir": str(lenv.repo), "branch": f"bg/localgremlin/{gr_id}",
        "stage": "address-code", "status": "done", "exit_code": 0,
        "pipeline_args": [],
    }
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")
    (state_dir / "finished").touch()
    (state_dir / "instructions.txt").write_text("foo", encoding="utf-8")

    with pytest.raises(RuntimeError, match="finished successfully"):
        launcher.resume(gr_id)


# ---------------------------------------------------------------------------
# _run-pipeline subcommand (terminal state)
# ---------------------------------------------------------------------------

def test_run_pipeline_writes_terminal_state_on_success(lenv):
    """_run-pipeline writes exit_code=0 + status=done + finished marker on success."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Test Plan\n\n## Tasks\n- [ ] Touch a file\n", encoding="utf-8")
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", plan=str(plan_file))
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=120), \
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    state = _read_state(state_dir)
    assert state["exit_code"] == 0
    assert state["status"] == "done"
    assert (state_dir / "finished").exists()


def test_run_pipeline_writes_terminal_state_on_failure(lenv, monkeypatch):
    """_run-pipeline writes exit_code!=0 + status=stopped + finished marker on failure."""
    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "plan")
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", instructions="fail test")
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=60), "pipeline should terminate quickly on failure"
    state = _read_state(state_dir)
    assert state["exit_code"] != 0
    assert state["status"] == "stopped"
    assert (state_dir / "finished").exists()


def test_run_pipeline_cleans_up_worktree_on_success(lenv):
    """On exit_code=0 and localgremlin, _run-pipeline removes the git worktree."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Plan\n\n## Tasks\n- [ ] x\n", encoding="utf-8")
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", plan=str(plan_file))
    state_dir = _gremlins_state_root(lenv) / gr_id
    state = _read_state(state_dir)
    workdir = pathlib.Path(state["workdir"])
    assert _wait_for_finished(state_dir, timeout=120)
    final_state = _read_state(state_dir)
    if final_state["exit_code"] == 0:
        assert _wait_for_workdir_removed(workdir, timeout=30), \
            "worktree should be removed after successful completion"


# ---------------------------------------------------------------------------
# Full pipeline smoke test
# ---------------------------------------------------------------------------

def test_full_localgremlin_pipeline(lenv):
    """plan → implement → review → address all run once in order."""
    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", instructions="test full pipeline")
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=120), \
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    state = _read_state(state_dir)
    assert state["status"] == "done", \
        f"expected done, got {state.get('status')}; log:\n" \
        f"{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    assert state["exit_code"] == 0
    # Check stage ordering from fake claude log
    if lenv.fake_claude_log.exists():
        log = [json.loads(line) for line in lenv.fake_claude_log.read_text().splitlines() if line.strip()]
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
    launcher = _launcher()
    gr_id = launcher.launch("ghgremlin", instructions="test gh launch")
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert state_dir.is_dir()

    state = _read_state(state_dir)
    assert state["id"] == gr_id
    assert state["kind"] == "ghgremlin"
    assert state["setup_kind"] == "worktree", \
        f"ghgremlin should use detached worktree, got: {state['setup_kind']!r}"
    assert state["worktree_base"] == "origin/main", \
        f"worktree_base should be origin/main, got: {state['worktree_base']!r}"
    workdir = pathlib.Path(state["workdir"])
    assert workdir.is_dir(), f"worktree directory should exist: {workdir}"

    _wait_for_finished(state_dir, timeout=60)


# ---------------------------------------------------------------------------
# PYTHONSAFEPATH worktree-rename regression
# ---------------------------------------------------------------------------

def test_launch_passes_base_ref_to_worktree_setup(lenv, monkeypatch):
    """launch(base_ref=<sha>) calls setup_worktree_branch with base_ref=<sha>."""
    from gremlins import git as git_mod

    captured = {}
    real_setup = git_mod.setup_worktree_branch

    def fake_setup(project_root, gr_id, base_ref="HEAD", branch_prefix="bg/localgremlin"):
        captured["base_ref"] = base_ref
        return real_setup(project_root, gr_id, base_ref=base_ref, branch_prefix=branch_prefix)

    monkeypatch.setattr(git_mod, "setup_worktree_branch", fake_setup)

    launcher = _launcher()
    # Use the actual HEAD SHA so worktree add succeeds.
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True, text=True, cwd=str(lenv.repo), check=True,
    )
    head_sha = r.stdout.strip()

    gr_id = launcher.launch("localgremlin", instructions="base_ref test", base_ref=head_sha)
    state_dir = _gremlins_state_root(lenv) / gr_id
    _wait_for_finished(state_dir, timeout=60)

    assert captured.get("base_ref") == head_sha, \
        f"expected base_ref={head_sha!r}, got {captured.get('base_ref')!r}"


def test_pipeline_survives_worktree_pipeline_rename(lenv, monkeypatch):
    """Regression: pipeline completes even when implement renames worktree's gremlins/.

    Without PYTHONSAFEPATH=1, python -m gremlins.cli imports from the worktree
    (cwd). Renaming gremlins/ during implement then causes ImportError / FileNotFoundError
    in later stages because PROMPTS_DIR is __file__-relative and the directory is gone.
    With the fix, python loads gremlins from HOME/.claude/gremlins/ and the
    worktree rename is harmless.
    """
    # Add a gremlins/ stub to the repo so the worktree shadows $HOME/.claude/gremlins/.
    pipeline_stub = lenv.repo / "gremlins"
    pipeline_stub.mkdir()
    (pipeline_stub / "__init__.py").write_text("# stub\n", encoding="utf-8")
    subprocess.run(
        ["git", "-C", str(lenv.repo), "add", "gremlins"],
        check=True, capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(lenv.repo), "commit", "-m", "add gremlins stub"],
        check=True, capture_output=True,
    )

    # FAKE_CLAUDE_RENAME_GREMLINS=1 triggers the rename inside handle_implement()
    # (see fixtures/fake_claude.py). Set it in the current env so the spawned
    # pipeline process inherits it via os.environ.copy() in _build_spawn_env().
    monkeypatch.setenv("FAKE_CLAUDE_RENAME_GREMLINS", "1")

    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", instructions="test gremlins rename regression")
    state_dir = _gremlins_state_root(lenv) / gr_id
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
# spec_path forwarding
# ---------------------------------------------------------------------------

def test_launch_threads_spec_path_into_pipeline_args(lenv):
    """launch(spec_path=<abs>) puts --spec <abs> into state.json pipeline_args."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Plan\n\nDo stuff.\n", encoding="utf-8")
    spec_file = lenv.repo / "spec.md"
    spec_file.write_text("the overall spec", encoding="utf-8")

    launcher = _launcher()
    gr_id = launcher.launch("localgremlin", plan=str(plan_file),
                            spec_path=str(spec_file))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert "--spec" in state["pipeline_args"]
    idx = state["pipeline_args"].index("--spec")
    assert state["pipeline_args"][idx + 1] == str(spec_file.resolve())
    _wait_for_finished(_gremlins_state_root(lenv) / gr_id, timeout=60)


def test_launch_rejects_missing_spec_path(lenv):
    """spec_path that doesn't exist raises ValueError before any state-dir setup."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Plan\n", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="--spec"):
        launcher.launch("localgremlin", plan=str(plan_file),
                        spec_path="/nonexistent/spec.md")
    dirs = list(_gremlins_state_root(lenv).glob("*")) \
        if _gremlins_state_root(lenv).exists() else []
    assert dirs == [], f"missing-spec failure must not create state: {dirs}"


def test_launch_rejects_empty_spec_path(lenv):
    """spec_path pointing to an empty file raises ValueError."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Plan\n", encoding="utf-8")
    spec_file = lenv.repo / "empty-spec.md"
    spec_file.write_text("", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="--spec"):
        launcher.launch("localgremlin", plan=str(plan_file),
                        spec_path=str(spec_file))
