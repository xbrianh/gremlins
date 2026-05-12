"""Tests for gremlins/launcher.py.

Drives launcher.launch() and launcher.resume() directly with:
- Monkeypatched HOME and PATH (so the spawned pipeline
  finds the fake `claude` binary and the gremlins package).
- Real throwaway git repos for worktree tests.

Does NOT use subprocess.run([launch.sh, ...]) — this is the Python replacement.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import shutil
import subprocess
import time

import platformdirs
import pytest
from fixtures.shell_env import install_fake_bin

import gremlins.utils.git as git_mod

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
FIXTURES_DIR = pathlib.Path(__file__).resolve().parent / "fixtures"
FAKE_CLAUDE = FIXTURES_DIR / "fake_claude.py"
FAKE_GH = FIXTURES_DIR / "fake_gh.py"


def _setup_claude_home(home: pathlib.Path) -> None:
    claude_dir = home / ".claude"
    claude_dir.mkdir(parents=True, exist_ok=True)
    for name in ("gremlins", "agents"):
        link = claude_dir / name
        if not link.exists() and not link.is_symlink():
            link.symlink_to(REPO_ROOT / name)


def _init_git_repo(path: pathlib.Path, *, with_origin: bool = False) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    (path / "README.md").write_text("init\n")
    subprocess.run(
        ["git", "add", "README.md"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True
    )
    if with_origin:
        bare = path.parent / f"{path.name}.git"
        subprocess.run(
            ["git", "init", "--bare", "-b", "main", str(bare)],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["git", "remote", "add", "origin", str(bare)],
            cwd=path,
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["git", "push", "-u", "origin", "main"],
            cwd=path,
            check=True,
            capture_output=True,
        )


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
def lenv(tmp_path, monkeypatch):
    """Launcher environment: isolated HOME, state root, git repo, fake claude."""
    home = tmp_path / "home"
    home.mkdir(parents=True, exist_ok=True)
    _setup_claude_home(home)
    monkeypatch.setenv("HOME", str(home))

    bin_dir = tmp_path / "bin"
    install_fake_bin(bin_dir, "claude", FAKE_CLAUDE)

    state_root = pathlib.Path(platformdirs.user_state_dir("gremlins"))
    state_root.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    repo = tmp_path / "repo"
    _init_git_repo(repo)

    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(tmp_path / "fake_claude.log"))
    monkeypatch.setenv("GIT_OPTIONAL_LOCKS", "0")
    monkeypatch.setenv("GREMLINS_TEST_NOOP_PIPELINE", "1")
    old_path = os.environ.get("PATH", "")
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{old_path}")
    monkeypatch.delenv("PYTHONPATH", raising=False)
    monkeypatch.delenv("GR_ID", raising=False)
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
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


class _FakeProc:
    pid = 12345


# ---------------------------------------------------------------------------
# launch() — basic contracts
# ---------------------------------------------------------------------------


def test_launch_returns_gr_id(lenv):
    """launch() returns a well-formed GR_ID string."""
    launcher = _launcher()
    gr_id = launcher.launch("local", stage_inputs={"instructions": "test instructions"})
    assert gr_id, "expected a non-empty GR_ID"
    assert re.match(r"^[a-z0-9-]+-[0-9a-f]{6}$", gr_id), (
        f"GR_ID has unexpected shape: {gr_id!r}"
    )
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )


def test_launch_creates_state_layout(lenv):
    """launch() creates state dir; initialize_runtime() fills in worktree and files."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        stage_inputs={"instructions": "test instructions"},
    )
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert state_dir.is_dir()
    sf = state_dir / "state.json"
    assert sf.exists()

    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )

    assert (state_dir / "instructions.txt").exists()
    assert (state_dir / "instructions.txt").read_text(
        encoding="utf-8"
    ) == "test instructions"

    state = _read_state(state_dir)
    assert state["id"] == gr_id
    assert state["kind"] == "local"
    assert state["setup_kind"] == "worktree-branch"
    assert state["pipeline_path"].endswith(".yaml")
    assert "test instructions" in state["instructions"]
    assert "workdir" in state and state["workdir"]


def test_launch_writes_worktree(lenv):
    """localgremlin creates a named-branch worktree via initialize_runtime()."""
    launcher = _launcher()
    gr_id = launcher.launch("local", stage_inputs={"instructions": "test"})
    state_dir = _gremlins_state_root(lenv) / gr_id
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
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        pipeline_args=("--client", "claude:opus"),
        stage_inputs={"instructions": "test"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["pipeline_path"].endswith(".yaml")
    assert state["pipeline_args"] == ["--client", "claude:opus"]


def test_launch_persists_pipeline_default_client(lenv):
    """The resolved pipeline default client is stored in state.json."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        stage_inputs={"instructions": "test default client"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["client"] == "claude:sonnet"


def test_launch_persists_cli_client_space_form(lenv):
    """A space-separated --client flag overrides the stored default client."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        pipeline_args=("--client", "copilot:gpt-5.4"),
        stage_inputs={"instructions": "test cli client space"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_persists_cli_client_equals_form(lenv):
    """An equals-form --client flag overrides the stored default client."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        pipeline_args=("--client=copilot:gpt-5.4",),
        stage_inputs={"instructions": "test cli client equals"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_persists_last_repeated_cli_client(lenv):
    """Repeated --client flags follow argparse's last-value-wins behavior."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        pipeline_args=(
            "--client",
            "claude:sonnet",
            "--client=copilot:gpt-5.4",
        ),
        stage_inputs={"instructions": "test repeated cli client"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_persists_custom_pipeline_default_client(lenv):
    """A custom pipeline stores the effective provider/default model label."""
    pipeline = lenv.repo / "custom.yaml"
    pipeline.write_text(
        """\
name: custom
default_client: copilot:gpt-5.4
stages:
  - name: implement
    type: implement
""",
        encoding="utf-8",
    )
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        pipeline_args=("--pipeline", str(pipeline)),
        stage_inputs={"instructions": "test custom pipeline client"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_ghgremlin_persists_pipeline_default_client(lenv_with_gh):
    """ghgremlin stores the default provider/model label."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "gh", stage_inputs={"instructions": "test gh default client"}
    )
    state = _read_state(_gremlins_state_root(lenv_with_gh) / gr_id)
    assert state["client"] == "claude:sonnet"


def test_launch_ghgremlin_persists_cli_client_override(lenv_with_gh):
    """ghgremlin stores an explicit --client override."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "gh",
        pipeline_args=("--client", "copilot:gpt-5.4"),
        stage_inputs={"instructions": "test gh cli client"},
    )
    state = _read_state(_gremlins_state_root(lenv_with_gh) / gr_id)
    assert state["client"] == "copilot:gpt-5.4"


def test_launch_plan_normalized_to_absolute(lenv):
    """A relative --plan path is resolved to absolute in state.json."""
    plan_file = lenv.repo / "my-plan.md"
    plan_file.write_text("# My Plan Heading\n\nBody.\n", encoding="utf-8")
    launcher = _launcher()
    gr_id = launcher.launch("local", plan=str(plan_file.name))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    idx = state["pipeline_args"].index("--plan")
    persisted = state["pipeline_args"][idx + 1]
    assert os.path.isabs(persisted), f"expected absolute path, got: {persisted!r}"
    assert pathlib.Path(persisted).name == "my-plan.md"
    assert state["description"].startswith("My Plan Heading")


def test_launch_h1_as_description(lenv):
    """H1 from --plan file becomes the gremlin description."""
    plan_file = lenv.repo / "plan-with-h1.md"
    plan_file.write_text(
        "# Hello World Feature\n\n## Tasks\n- [ ] Do it\n", encoding="utf-8"
    )
    launcher = _launcher()
    gr_id = launcher.launch("local", plan=str(plan_file))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["description"].startswith("Hello World Feature")


def test_launch_invalid_pipeline_name_raises(lenv):
    """launch() raises FileNotFoundError for an unresolvable pipeline name."""
    launcher = _launcher()
    with pytest.raises(FileNotFoundError):
        launcher.launch("notapipeline", stage_inputs={"instructions": "test"})


def test_launch_plan_and_instructions_mutex(lenv):
    """launch() raises ValueError when both plan and instructions are given."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# X\n", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="mutually exclusive"):
        launcher.launch(
            "local", plan=str(plan_file), stage_inputs={"instructions": "extra"}
        )


def test_launch_empty_plan_file_rejected(lenv):
    """localgremlin rejects an empty --plan file before creating state."""
    empty = lenv.repo / "empty-plan.md"
    empty.write_text("", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="empty"):
        launcher.launch("local", plan=str(empty))
    # No state dir should have been created
    dirs = (
        list((_gremlins_state_root(lenv)).glob("*"))
        if _gremlins_state_root(lenv).exists()
        else []
    )
    assert dirs == [], f"empty-plan failure must not create state: {dirs}"


def test_launch_spawned_process_detached(lenv):
    """The spawned pipeline has a different process group than the parent."""
    launcher = _launcher()
    gr_id = launcher.launch("local", stage_inputs={"instructions": "pgid test"})
    state_dir = _gremlins_state_root(lenv) / gr_id
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


def test_launch_concurrent_no_collision(lenv):
    """Concurrent launches produce distinct GR_IDs."""
    launcher = _launcher()
    ids = [
        launcher.launch("local", stage_inputs={"instructions": f"concurrent {i}"})
        for i in range(5)
    ]
    assert len(set(ids)) == len(ids), f"GR_ID collision among: {ids}"


def test_launch_explicit_project_root(lenv):
    """Explicit project_root param is used; parent_id is recorded in state.json."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    parent_id = "fake-parent-aabbcc"
    gr_id = launcher.launch(
        "local",
        stage_inputs={"instructions": "child test"},
        parent_id=parent_id,
        project_root=str(lenv.repo),
    )
    state = _read_state(state_root / gr_id)
    assert state["project_root"] == str(lenv.repo)
    assert state["parent_id"] == parent_id


# ---------------------------------------------------------------------------
# resume() — state patches and guards
# ---------------------------------------------------------------------------


def test_resume_patches_state(lenv, monkeypatch):
    """resume() bumps rescue_count, clears markers, and patches state.json."""
    monkeypatch.delenv("GREMLINS_TEST_NOOP_PIPELINE")
    launcher = _launcher()
    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "plan")
    gr_id = launcher.launch("local", stage_inputs={"instructions": "test resume"})
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=30), (
        "failed gremlin should terminate quickly"
    )

    pre_state = _read_state(state_dir)
    pre_rescue_count = pre_state.get("rescue_count", 0)

    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "")
    launcher.resume(gr_id)

    post_state = _read_state(state_dir)
    assert post_state["rescue_count"] == pre_rescue_count + 1
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
    type: plan
  - name: implement
    type: implement
    client: claude:opus
""",
        encoding="utf-8",
    )
    launcher = _launcher()
    gr_id = "resume-stage-client"
    state_dir = _gremlins_state_root(lenv) / gr_id
    state_dir.mkdir(parents=True)
    (state_dir / "log").write_text("", encoding="utf-8")
    (state_dir / "finished").touch()
    (state_dir / "instructions.txt").write_text("test resume refresh", encoding="utf-8")
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
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

    monkeypatch.setattr(launcher, "_spawn_pipeline", lambda *args, **kwargs: _Proc())

    launcher.resume(gr_id)

    post_state = _read_state(state_dir)
    assert post_state["client"] == "claude:opus"
    assert post_state["status"] == "running"
    assert post_state["resumed_from_stage"] == "implement"
    assert post_state["pipeline_path"] == str(old_pipeline.resolve())
    assert not (state_dir / "finished").exists()


def test_resume_keeps_resume_flag_for_pipeline_gremlin(lenv, monkeypatch):
    launcher = _launcher()
    gr_id = "resume-local-spawn-args"
    state_dir = _gremlins_state_root(lenv) / gr_id
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "kind": "localgremlin",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "implement",
                "status": "stopped",
                "exit_code": 1,
                "pipeline_args": ["--pipeline", "local"],
            }
        ),
        encoding="utf-8",
    )
    (state_dir / "instructions.txt").write_text("resume instructions", encoding="utf-8")

    captured: dict[str, object] = {}

    class _Proc:
        pid = 12345

    def fake_spawn(state_dir, workdir, spawn_gr_id, subcommand, spawn_args, **kwargs):
        captured["subcommand"] = subcommand
        captured["spawn_args"] = list(spawn_args)
        return _Proc()

    monkeypatch.setattr(launcher, "_spawn_pipeline", fake_spawn)

    launcher.resume(gr_id)

    from gremlins.pipeline.discovery import resolve_pipeline_path

    expected_pipeline = str(resolve_pipeline_path("local", lenv.repo))
    assert captured["subcommand"] == expected_pipeline
    assert captured["spawn_args"] == [
        "--resume-from",
        "implement",
        "resume instructions",
    ]


def test_resume_bossgremlin_resumes_at_chain_stage(lenv, monkeypatch):
    """bossgremlin resume always uses --resume-from chain (unless post-chain stage)."""
    launcher = _launcher()
    gr_id = "resume-boss-spawn-args"
    state_dir = _gremlins_state_root(lenv) / gr_id
    state_dir.mkdir(parents=True)
    plan_path = lenv.repo / "boss-spec.md"
    plan_path.write_text("# Boss spec\n", encoding="utf-8")
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "kind": "bossgremlin",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "implement",
                "status": "stopped",
                "exit_code": 1,
                "pipeline_args": [
                    "--pipeline",
                    "boss",
                    "--plan",
                    str(plan_path),
                ],
            }
        ),
        encoding="utf-8",
    )

    captured: dict[str, object] = {}

    class _Proc:
        pid = 12345

    def fake_spawn(state_dir, workdir, spawn_gr_id, subcommand, spawn_args, **kwargs):
        captured["subcommand"] = subcommand
        captured["spawn_args"] = list(spawn_args)
        return _Proc()

    monkeypatch.setattr(launcher, "_spawn_pipeline", fake_spawn)

    launcher.resume(gr_id)

    from gremlins.pipeline.discovery import resolve_pipeline_path

    expected_pipeline = str(resolve_pipeline_path("boss", lenv.repo))
    assert captured["subcommand"] == expected_pipeline
    spawn_args = captured["spawn_args"]
    # --resume-from chain must be present (boss maps mid-chain stages back to "chain")
    assert "--resume-from" in spawn_args
    idx = spawn_args.index("--resume-from")
    assert spawn_args[idx + 1] == "chain"


def test_resume_refuses_running_gremlin(lenv):
    """resume() raises RuntimeError when the recorded pid is still alive."""
    launcher = _launcher()
    state_root = _gremlins_state_root(lenv)
    gr_id = "fake-running-deadbe"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    state = {
        "id": gr_id,
        "kind": "localgremlin",
        "workdir": str(lenv.repo),
        "branch": f"bg/local/{gr_id}",
        "stage": "plan",
        "status": "running",
        "pid": os.getpid(),
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
        "id": gr_id,
        "kind": "localgremlin",
        "workdir": str(lenv.repo),
        "branch": f"bg/local/{gr_id}",
        "stage": "address-code",
        "status": "done",
        "exit_code": 0,
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


def test_run_pipeline_writes_terminal_state_on_success(lenv, monkeypatch):
    """_run-pipeline writes exit_code=0 + status=done + finished marker on success."""
    monkeypatch.delenv("GREMLINS_TEST_NOOP_PIPELINE")
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text(
        "# Test Plan\n\n## Tasks\n- [ ] Touch a file\n", encoding="utf-8"
    )
    launcher = _launcher()
    gr_id = launcher.launch("local", plan=str(plan_file))
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=120), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )
    state = _read_state(state_dir)
    assert state["exit_code"] == 0
    assert state["status"] == "done"
    assert (state_dir / "finished").exists()


def test_run_pipeline_writes_terminal_state_on_failure(lenv, monkeypatch):
    """_run-pipeline writes exit_code!=0 + status=stopped + finished marker on failure."""
    monkeypatch.delenv("GREMLINS_TEST_NOOP_PIPELINE")
    monkeypatch.setenv("FAKE_CLAUDE_FAIL_AT", "plan")
    launcher = _launcher()
    gr_id = launcher.launch("local", stage_inputs={"instructions": "fail test"})
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=60), (
        "pipeline should terminate quickly on failure"
    )
    state = _read_state(state_dir)
    assert state["exit_code"] != 0
    assert state["status"] == "stopped"
    assert (state_dir / "finished").exists()


def test_write_terminal_state_preserves_worktree_for_gh(lenv, monkeypatch, tmp_path):
    """On success, worktree is NOT removed for gh-mode pipelines (only explicit close/land removes it)."""
    removed = []
    monkeypatch.setattr(
        "gremlins.utils.git.remove_worktree", lambda root, wd: removed.append(wd)
    )

    launcher = _launcher()
    state_dir = lenv.state_root / "test-gr-id-abc123"
    state_dir.mkdir(parents=True, exist_ok=True)
    fake_workdir = tmp_path / "workdir"
    fake_workdir.mkdir()
    state_json = {
        "project_root": str(lenv.repo),
        "workdir": str(fake_workdir),
    }
    (state_dir / "state.json").write_text(json.dumps(state_json), encoding="utf-8")

    launcher.write_terminal_state("test-gr-id-abc123", exit_code=0)

    assert removed == [], "worktree must not be removed on exit"


def test_write_terminal_state_preserves_worktree_for_local(lenv, monkeypatch, tmp_path):
    """On success, worktree is NOT removed for local-mode pipelines."""
    removed = []
    monkeypatch.setattr(
        "gremlins.utils.git.remove_worktree", lambda root, wd: removed.append(wd)
    )

    launcher = _launcher()
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

    launcher.write_terminal_state("test-gr-id-def456", exit_code=0)

    assert removed == [], "worktree must not be removed for local-mode pipelines"


def test_write_terminal_state_preserves_worktree_for_boss(lenv, monkeypatch, tmp_path):
    """On success, worktree is NOT removed for boss-mode pipelines."""
    removed = []
    monkeypatch.setattr(
        "gremlins.utils.git.remove_worktree", lambda root, wd: removed.append(wd)
    )

    launcher = _launcher()
    state_dir = lenv.state_root / "test-gr-id-ghi789"
    state_dir.mkdir(parents=True, exist_ok=True)
    fake_workdir = tmp_path / "workdir"
    fake_workdir.mkdir()
    state_json = {
        "project_root": str(lenv.repo),
        "workdir": str(fake_workdir),
        "setup_kind": "worktree",
    }
    (state_dir / "state.json").write_text(json.dumps(state_json), encoding="utf-8")

    launcher.write_terminal_state("test-gr-id-ghi789", exit_code=0)

    assert removed == [], "worktree must not be removed on exit"


# ---------------------------------------------------------------------------
# Full pipeline smoke test
# ---------------------------------------------------------------------------


def test_full_localgremlin_pipeline(lenv, monkeypatch):
    """plan → implement → review → address all run once in order."""
    monkeypatch.delenv("GREMLINS_TEST_NOOP_PIPELINE")
    launcher = _launcher()
    gr_id = launcher.launch(
        "local", stage_inputs={"instructions": "test full pipeline"}
    )
    state_dir = _gremlins_state_root(lenv) / gr_id
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
    launcher = _launcher()
    gr_id = launcher.launch("gh", stage_inputs={"instructions": "test gh launch"})
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert state_dir.is_dir()

    assert _wait_for_finished(state_dir, timeout=120), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )

    state = _read_state(state_dir)
    assert state["id"] == gr_id
    assert state["kind"] == "gh"
    assert state["setup_kind"] == "worktree", (
        f"ghgremlin should use detached worktree, got: {state['setup_kind']!r}"
    )
    assert state["base_ref_name"] == "main", (
        f"base_ref_name should be 'main', got: {state['base_ref_name']!r}"
    )
    assert len(state.get("base_ref_sha", "")) == 40, (
        f"base_ref_sha should be a 40-char SHA, got: {state.get('base_ref_sha')!r}"
    )
    assert len(state.get("worktree_base", "")) == 40, (
        f"worktree_base should be a SHA, got: {state.get('worktree_base')!r}"
    )
    workdir = pathlib.Path(state["workdir"])
    assert workdir.is_dir(), f"worktree directory should exist: {workdir}"


def test_launch_gh_plan_issue_ref_not_snapshotted(lenv_with_gh):
    """gh pipeline with --plan <issue-ref> keeps the raw ref without snapshotting."""
    lenv = lenv_with_gh
    launcher = _launcher()
    gr_id = launcher.launch("gh", plan="#42")
    state_dir = _gremlins_state_root(lenv) / gr_id
    state = _read_state(state_dir)

    idx = state["pipeline_args"].index("--plan")
    persisted = state["pipeline_args"][idx + 1]
    assert persisted == "#42", f"expected raw issue ref '#42', got: {persisted!r}"
    assert not (state_dir / "plan-from-issue.md").exists()


# ---------------------------------------------------------------------------
# PYTHONSAFEPATH worktree-rename regression
# ---------------------------------------------------------------------------


def test_launch_passes_base_ref_to_worktree_setup(lenv):
    """launch(base_ref=<sha>) passes the sha to the worktree setup and persists it."""
    launcher = _launcher()
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        cwd=str(lenv.repo),
        check=True,
    )
    head_sha = r.stdout.strip()

    gr_id = launcher.launch(
        "local", stage_inputs={"instructions": "base_ref test"}, base_ref=head_sha
    )
    state_dir = _gremlins_state_root(lenv) / gr_id
    assert _wait_for_finished(state_dir, timeout=60), (
        f"pipeline did not finish; log:\n{(state_dir / 'log').read_text(errors='replace')[-2000:]}"
    )

    state = _read_state(state_dir)
    assert state.get("base_ref_sha") == head_sha, (
        f"expected base_ref_sha={head_sha!r}, got {state.get('base_ref_sha')!r}"
    )
    workdir = state.get("workdir", "")
    assert workdir, "workdir should be set after initialize_runtime"
    branch_base = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        cwd=workdir,
        check=True,
    ).stdout.strip()
    assert branch_base == head_sha, (
        f"worktree branch should be based on {head_sha!r}, got {branch_base!r}"
    )


def test_pipeline_survives_worktree_pipeline_rename(lenv, monkeypatch):
    """Regression: pipeline completes even when implement renames worktree's gremlins/.

    Without PYTHONSAFEPATH=1, python -m gremlins.cli imports from the worktree
    (cwd). Renaming gremlins/ during implement then causes ImportError / FileNotFoundError
    in later stages because PROMPTS_DIR is __file__-relative and the directory is gone.
    With the fix, python loads gremlins from the install root (derived via __file__)
    and the worktree rename is harmless.
    """
    monkeypatch.delenv("GREMLINS_TEST_NOOP_PIPELINE")
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

    launcher = _launcher()
    gr_id = launcher.launch(
        "local", stage_inputs={"instructions": "test gremlins rename regression"}
    )
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
# .gremlins overlay placement
# ---------------------------------------------------------------------------


def test_setup_workdir_overlay_goes_to_state_dir(lenv):
    """stage_gremlins_overlay copies .gremlins to state_dir, not the worktree."""
    overlay_src = lenv.repo / ".gremlins"
    overlay_src.mkdir(parents=True)
    (overlay_src / "custom-local.yaml").write_text(
        "name: custom-local\nstages: []\n", encoding="utf-8"
    )

    gr_id = "overlay-test-aabbcc"
    state_dir = lenv.state_root / gr_id
    state_dir.mkdir(parents=True)

    workdir, _branch, _wt_base, _kind = git_mod.setup_workdir(
        "local", str(lenv.repo), "HEAD", gr_id, state_dir
    )

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
    gr_id = launcher.launch("local", plan=str(plan_file), spec_path=str(spec_file))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert "--spec" in state["pipeline_args"]
    idx = state["pipeline_args"].index("--spec")
    assert state["pipeline_args"][idx + 1] == str(spec_file.resolve())


def test_launch_rejects_missing_spec_path(lenv):
    """spec_path that doesn't exist raises ValueError before any state-dir setup."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Plan\n", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="--spec"):
        launcher.launch("local", plan=str(plan_file), spec_path="/nonexistent/spec.md")
    dirs = (
        list(_gremlins_state_root(lenv).glob("*"))
        if _gremlins_state_root(lenv).exists()
        else []
    )
    assert dirs == [], f"missing-spec failure must not create state: {dirs}"


def test_launch_rejects_empty_spec_path(lenv):
    """spec_path pointing to an empty file raises ValueError."""
    plan_file = lenv.repo / "plan.md"
    plan_file.write_text("# Plan\n", encoding="utf-8")
    spec_file = lenv.repo / "empty-spec.md"
    spec_file.write_text("", encoding="utf-8")
    launcher = _launcher()
    with pytest.raises(ValueError, match="--spec"):
        launcher.launch("local", plan=str(plan_file), spec_path=str(spec_file))


# ---------------------------------------------------------------------------
# stage_inputs persistence
# ---------------------------------------------------------------------------


def test_launch_persists_stage_inputs(lenv):
    """stage_inputs dict is written verbatim to state.json."""
    launcher = _launcher()
    gr_id = launcher.launch(
        "local",
        stage_inputs={"instructions": "do the thing", "extra_key": "val"},
    )
    state = _read_state(_gremlins_state_root(lenv) / gr_id)
    assert state["stage_inputs"] == {"instructions": "do the thing", "extra_key": "val"}


def test_stage_inputs_survives_resume(lenv, monkeypatch):
    """stage_inputs written at launch is still present in state.json after resume."""
    launcher = _launcher()
    gr_id = "resume-stage-inputs-roundtrip"
    state_dir = _gremlins_state_root(lenv) / gr_id
    state_dir.mkdir(parents=True)
    (state_dir / "log").write_text("", encoding="utf-8")
    (state_dir / "finished").touch()
    (state_dir / "instructions.txt").write_text("do the thing", encoding="utf-8")
    saved_stage_inputs = {"instructions": "do the thing", "flag": "value"}
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": gr_id,
                "kind": "localgremlin",
                "workdir": str(lenv.repo),
                "project_root": str(lenv.repo),
                "stage": "plan",
                "status": "stopped",
                "exit_code": 1,
                "pipeline_args": ["--pipeline", str(lenv.repo / "pipeline.yaml")],
                "pipeline_path": str(lenv.repo / "pipeline.yaml"),
                "stage_inputs": saved_stage_inputs,
            }
        ),
        encoding="utf-8",
    )

    class _Proc:
        pid = 99999

    monkeypatch.setattr(launcher, "_spawn_pipeline", lambda *args, **kwargs: _Proc())

    launcher.resume(gr_id)

    post_state = _read_state(state_dir)
    assert post_state["stage_inputs"] == saved_stage_inputs


# ---------------------------------------------------------------------------
# Boss pipeline + issue-ref plan materializes plan.md
# ---------------------------------------------------------------------------


def test_launch_boss_plan_issue_ref_materializes_plan_md(lenv, monkeypatch):
    """Boss + --plan #N writes the issue body to artifacts/plan.md before chain runs."""
    launcher = _launcher()

    issue_body = "# My Plan\n\nDo the thing."
    monkeypatch.setattr(
        launcher,
        "fetch_issue",
        lambda plan: {"title": "My Plan", "body": issue_body, "number": 317, "url": ""},
    )

    monkeypatch.setattr(
        launcher, "_spawn_pipeline", lambda *args, **kwargs: _FakeProc()
    )

    gr_id = launcher.launch("boss", plan="#317", project_root=str(lenv.repo))
    state_dir = _gremlins_state_root(lenv) / gr_id
    plan_md = state_dir / "artifacts" / "plan.md"

    assert plan_md.exists(), f"plan.md not found at {plan_md}"
    assert plan_md.read_text(encoding="utf-8").strip() == issue_body.strip()


def test_launch_plan_issue_ref_writes_issue_url_and_num(lenv, monkeypatch):
    launcher = _launcher()

    monkeypatch.setattr(
        launcher,
        "fetch_issue",
        lambda plan: {
            "title": "My Plan",
            "body": "# My Plan\n\nDo the thing.",
            "number": 378,
            "url": "https://github.com/owner/repo/issues/378",
        },
    )

    monkeypatch.setattr(
        launcher, "_spawn_pipeline", lambda *args, **kwargs: _FakeProc()
    )

    gr_id = launcher.launch("boss", plan="#378", project_root=str(lenv.repo))
    state = _read_state(_gremlins_state_root(lenv) / gr_id)

    assert state["issue_url"] == "https://github.com/owner/repo/issues/378"
    assert state["issue_num"] == "378"
