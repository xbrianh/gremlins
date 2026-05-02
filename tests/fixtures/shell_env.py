"""Helpers for shell integration tests.

Each test gets its own isolated HOME, XDG_STATE_HOME, PATH, and a real git
repo. ``setup_shell_env`` returns a populated ``ShellEnv`` dataclass with
paths the test needs to assert against.
"""

from __future__ import annotations

import dataclasses
import json
import os
import pathlib
import shlex
import subprocess
import sys
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent.parent
FIXTURES_DIR = pathlib.Path(__file__).resolve().parent
FAKE_CLAUDE = FIXTURES_DIR / "fake_claude.py"
FAKE_GH = FIXTURES_DIR / "fake_gh.py"


@dataclasses.dataclass
class ShellEnv:
    home: pathlib.Path
    bin_dir: pathlib.Path
    state_root: pathlib.Path
    repo: pathlib.Path
    env: dict
    fake_claude_log: pathlib.Path
    fake_gh_log: pathlib.Path


def install_fake_bin(bin_dir: pathlib.Path, name: str, target: pathlib.Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    wrapper = bin_dir / name
    # Quote both paths in case sys.executable or target contains spaces
    # (e.g. macOS framework Pythons or custom install locations).
    wrapper.write_text(
        f"#!/usr/bin/env bash\nexec {shlex.quote(sys.executable)} {shlex.quote(str(target))} \"$@\"\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o755)


def setup_fake_home(home: pathlib.Path) -> None:
    """Build a minimal ``$HOME/.claude/`` that the gremlins package can
    resolve: gremlins/, agents/ symlink to the repo's source dirs."""
    claude_dir = home / ".claude"
    claude_dir.mkdir(parents=True, exist_ok=True)
    for name in ("gremlins", "agents"):
        link = claude_dir / name
        if link.is_symlink() or link.exists():
            continue
        link.symlink_to(REPO_ROOT / name)


def init_git_repo(path: pathlib.Path, *, with_origin: bool = False) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "test@test.com"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=path, check=True, capture_output=True)
    (path / "README.md").write_text("init\n")
    subprocess.run(["git", "add", "README.md"], cwd=path, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True)

    if with_origin:
        # Create a bare repo as `origin` and push so ghgremlin's
        # `git fetch origin <default>` and `worktree add origin/<default>`
        # have something real to resolve against.
        bare = path.parent / f"{path.name}.git"
        subprocess.run(["git", "init", "--bare", "-b", "main", str(bare)], check=True, capture_output=True)
        subprocess.run(["git", "remote", "add", "origin", str(bare)], cwd=path, check=True, capture_output=True)
        subprocess.run(["git", "push", "-u", "origin", "main"], cwd=path, check=True, capture_output=True)


def setup_shell_env(
    tmp_path: pathlib.Path,
    *,
    with_gh: bool = False,
    init_repo: bool = True,
    with_origin: bool = False,
) -> ShellEnv:
    home = tmp_path / "home"
    home.mkdir(parents=True, exist_ok=True)
    setup_fake_home(home)

    bin_dir = tmp_path / "bin"
    install_fake_bin(bin_dir, "claude", FAKE_CLAUDE)
    if with_gh:
        install_fake_bin(bin_dir, "gh", FAKE_GH)

    state_root = tmp_path / "state"
    state_root.mkdir(parents=True, exist_ok=True)

    repo = tmp_path / "repo"
    if init_repo:
        init_git_repo(repo, with_origin=with_origin)

    fake_claude_log = tmp_path / "fake_claude.log"
    fake_gh_log = tmp_path / "fake_gh.log"

    env = {
        **os.environ,
        "HOME": str(home),
        "XDG_STATE_HOME": str(state_root),
        "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
        "FAKE_CLAUDE_LOG": str(fake_claude_log),
        "FAKE_GH_LOG": str(fake_gh_log),
        # Strip caller's PYTHONPATH; the launcher will set its own.
    }
    env.pop("PYTHONPATH", None)
    # Disable git's optional locks to reduce lock contention across parallel
    # test runs (e.g. read-only commands like `git status` skip the index lock).
    env["GIT_OPTIONAL_LOCKS"] = "0"
    return ShellEnv(
        home=home, bin_dir=bin_dir, state_root=state_root, repo=repo,
        env=env, fake_claude_log=fake_claude_log, fake_gh_log=fake_gh_log,
    )


def wait_for_finished(state_dir: pathlib.Path, timeout: float = 60.0) -> bool:
    """Poll for both ``state_dir/finished`` and a terminal status in state.json.

    finish.sh writes the ``finished`` marker before patching state.json, so
    polling only for the marker leaves a small race window where
    state["status"] is still "running". This helper waits for both.

    Returns True once both conditions are met within ``timeout``, False
    if the deadline expires first.
    """
    finished = state_dir / "finished"
    state_file = state_dir / "state.json"
    deadline = time.time() + timeout
    while time.time() < deadline:
        if finished.exists():
            try:
                data = json.loads(state_file.read_text(encoding="utf-8"))
                if data.get("status") not in ("running", "starting", None, ""):
                    return True
            except Exception:
                pass
        time.sleep(0.1)
    return False


def read_fake_claude_log(log_path: pathlib.Path) -> list:
    if not log_path.exists():
        return []
    out = []
    for line in log_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except Exception:
            continue
    return out


def read_state(state_file: pathlib.Path) -> dict:
    return json.loads(state_file.read_text(encoding="utf-8"))
